use reqwest::{Certificate, Client, ClientBuilder};
use rustls_native_certs::CertificateResult;
use rustls_pki_types::CertificateDer;

#[derive(Debug, thiserror::Error)]
pub enum ProviderHttpError {
    #[error("native system trust store contained no usable certificates: {errors}")]
    UnusableNativeRoots {
        #[source]
        errors: NativeRootErrors,
    },
    #[error("native system trust store certificate could not be added to the HTTPS client: {0}")]
    InvalidNativeRoot(#[source] reqwest::Error),
    #[error("provider HTTPS client could not be built: {0}")]
    ClientBuild(#[source] reqwest::Error),
}

#[derive(Debug)]
pub struct NativeRootErrors(Vec<rustls_native_certs::Error>);

impl std::error::Error for NativeRootErrors {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.first().map(|error| error as _)
    }
}

impl std::fmt::Display for NativeRootErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_slice() {
            [] => f.write_str("no loader errors were reported"),
            [error] => write!(f, "one entry failed to load: {error}"),
            errors => {
                write!(f, "{} entries failed to load: ", errors.len())?;
                for (index, error) in errors.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{error}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeRootReport {
    pub loaded: usize,
    pub rejected: usize,
}

/// Builds a provider HTTP client with bundled public roots plus the platform trust store.
///
/// Loading stays here, in the network-owning provider layer. Invalid individual native
/// entries are reported and ignored by `rustls-native-certs`; an entirely unusable native
/// store is a startup error rather than a silent fallback to public roots only.
pub fn client_builder() -> Result<(ClientBuilder, NativeRootReport), ProviderHttpError> {
    builder_from_native_roots(rustls_native_certs::load_native_certs())
}

pub fn client() -> Result<Client, ProviderHttpError> {
    let (builder, _) = client_builder()?;
    builder.build().map_err(ProviderHttpError::ClientBuild)
}

fn builder_from_native_roots(
    roots: CertificateResult,
) -> Result<(ClientBuilder, NativeRootReport), ProviderHttpError> {
    let report = NativeRootReport {
        loaded: roots.certs.len(),
        rejected: roots.errors.len(),
    };
    if roots.certs.is_empty() {
        return Err(ProviderHttpError::UnusableNativeRoots {
            errors: NativeRootErrors(roots.errors),
        });
    }

    if report.rejected > 0 {
        tracing::warn!(
            loaded = report.loaded,
            rejected = report.rejected,
            "loaded system trust roots with rejected entries"
        );
    } else {
        tracing::debug!(loaded = report.loaded, "loaded system trust roots");
    }

    let builder = add_native_roots(reqwest::Client::builder(), roots.certs)?;
    Ok((builder, report))
}

/// Adds already-loaded native roots without disabling reqwest's bundled WebPKI roots.
/// This is public so provider-independent transport tests can exercise the trust boundary.
#[doc(hidden)]
pub fn add_native_roots(
    mut builder: ClientBuilder,
    roots: Vec<CertificateDer<'static>>,
) -> Result<ClientBuilder, ProviderHttpError> {
    // reqwest's rustls WebPKI roots remain enabled; native roots are additive.
    for root in roots {
        let certificate =
            Certificate::from_der(root.as_ref()).map_err(ProviderHttpError::InvalidNativeRoot)?;
        builder = builder.add_root_certificate(certificate);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_native_store_is_rejected_without_loader_errors() {
        let error = builder_from_native_roots(CertificateResult::default())
            .expect_err("empty native store must not fall back to bundled roots only");
        let ProviderHttpError::UnusableNativeRoots { errors } = &error else {
            panic!("unexpected error: {error}");
        };
        assert!(errors.0.is_empty());
        assert_eq!(
            error.to_string(),
            "native system trust store contained no usable certificates: no loader errors were reported"
        );
    }

    #[test]
    fn unusable_native_store_preserves_and_displays_loader_errors() {
        let mut roots = CertificateResult::default();
        roots.errors.push(rustls_native_certs::Error {
            context: "failed to load certificate file",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
                path: "/private/cert.pem".into(),
            },
        });
        roots.errors.push(rustls_native_certs::Error {
            context: "failed to load certificate directory",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
                path: "/private/certs".into(),
            },
        });

        let error = builder_from_native_roots(roots)
            .expect_err("native store with only loader errors must be rejected");
        let ProviderHttpError::UnusableNativeRoots { errors } = &error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(errors.0.len(), 2);
        assert_eq!(
            std::error::Error::source(errors)
                .expect("first loader error remains in the source chain")
                .to_string(),
            "failed to load certificate file: access denied at '/private/cert.pem'"
        );
        assert_eq!(
            error.to_string(),
            "native system trust store contained no usable certificates: 2 entries failed to load: failed to load certificate file: access denied at '/private/cert.pem'; failed to load certificate directory: missing at '/private/certs'"
        );
    }
}
