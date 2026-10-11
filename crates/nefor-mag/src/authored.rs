use std::fmt;

/// A lexically authored name path. Segments are retained so punctuation inside
/// a quoted name remains one segment rather than being reinterpreted as qualification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamePath {
    segments: Vec<String>,
    qualified: String,
}

impl NamePath {
    pub fn single(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            qualified: name.clone(),
            segments: vec![name],
        }
    }

    pub fn from_segments(segments: Vec<String>) -> Self {
        debug_assert!(!segments.is_empty());
        let qualified = segments.join(".");
        Self {
            segments,
            qualified,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.qualified
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn last(&self) -> &str {
        self.segments.last().map(String::as_str).unwrap_or("")
    }

    pub fn owner(&self) -> Option<Self> {
        (self.segments.len() > 1)
            .then(|| Self::from_segments(self.segments[..self.segments.len() - 1].to_vec()))
    }
}

impl fmt::Display for NamePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.qualified)
    }
}

impl std::ops::Deref for NamePath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl PartialEq<str> for NamePath {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for NamePath {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<String> for NamePath {
    fn from(name: String) -> Self {
        Self::single(name)
    }
}

impl From<&str> for NamePath {
    fn from(name: &str) -> Self {
        Self::single(name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub forms: Vec<Form>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Form {
    Require(Require),
    Type(TypeDeclaration),
    Block(BlockItem),
    Invalid(AuthoringError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Require {
    pub module: String,
    pub exposure: ImportExposure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportExposure {
    Open,
    Qualified,
    Selective(Vec<ImportSelector>),
    NamespaceAlias(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSelector {
    pub export: String,
    pub local: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeDeclaration {
    pub name: String,
    pub params: Vec<String>,
    pub body: TypeDeclarationBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeDeclarationBody {
    Fields(Vec<(String, Type)>),
    TransparentAlias(Type),
    Newtype(Type),
    Adt(Vec<ConstructorDeclaration>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstructorDeclaration {
    pub name: String,
    pub payload: Type,
    /// Declared as a bare name, with no payload. Its payload type is `Unit`,
    /// so its identity equals that of `Name(Unit)`; only its spelling differs.
    pub payloadless: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockItem {
    Let { name: String, value: Expr },
    Expr(Expr),
    Invalid(AuthoringError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Unit,
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Keyword(String),
    Name(NamePath),
    Vector(Vec<Expr>),
    /// A tuple whose element types are not evident from syntax alone; the
    /// checker gives it the product of its elements' types.
    Tuple(Vec<Expr>),
    Fields(Vec<(String, Expr)>),
    If {
        condition: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// `Owner.Constructor(arguments)` whose owner is syntactically a type. A
    /// plain dotted call stays a `Call`; the checker decides after imports
    /// whether its path names a function or a constructor.
    Construct {
        owner: Type,
        constructor: String,
        arguments: ConstructorArguments,
    },
    Match {
        value: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Call {
        callee: Box<Expr>,
        type_args: Option<Vec<TypeArgument>>,
        args: Vec<Expr>,
    },
    Function(Function),
    Ascribe {
        target: Type,
        value: Box<Expr>,
    },
    Annotate {
        target: Type,
        value: Box<Expr>,
    },
    TypeTag(Type),
    Invalid(AuthoringError),
}

/// Constructor arguments as written. The checker shapes them into the payload
/// once the constructor's declared payload type is known.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstructorArguments {
    Positional(Vec<Expr>),
    Named(Vec<(String, Expr)>),
    /// The payload value itself, with no argument sugar (the Lisp
    /// `construct` form, which always states its payload).
    Payload(Box<Expr>),
    /// No argument list at all: `Owner<Args>.Constructor`.
    Bare,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeArgument {
    Explicit(Type),
    Infer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: NamePath,
    /// The payload binding; absent for `case Constructor =>`.
    pub binding: Option<String>,
    pub body: Box<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub type_params: Vec<String>,
    pub params: Vec<Parameter>,
    pub result: Type,
    pub body: Vec<BlockItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Name(NamePath),
    Product(Vec<Type>),
    Tag(Box<Type>),
    Function {
        params: Vec<Type>,
        result: Box<Type>,
    },
    Apply {
        constructor: NamePath,
        arguments: Vec<Type>,
    },
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthoringError {
    Type(String),
    Eval(String),
    Arity { expected: usize, got: usize },
}

impl AuthoringError {
    pub fn into_mag_error(self) -> crate::error::MagError {
        match self {
            Self::Type(message) => crate::error::MagError::Type(message),
            Self::Eval(message) => crate::error::MagError::Eval(message),
            Self::Arity { expected, got } => crate::error::MagError::Arity { expected, got },
        }
    }
}
