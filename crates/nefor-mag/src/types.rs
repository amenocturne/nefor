use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MagType {
    Named(String),
    Var(String),
    Union(Vec<MagType>),
    Intersection(Vec<MagType>),
}

impl MagType {
    pub fn named(s: impl Into<String>) -> Self {
        Self::Named(s.into())
    }

    pub fn var(s: impl Into<String>) -> Self {
        Self::Var(s.into())
    }

    pub fn union(types: Vec<MagType>) -> Self {
        if types.len() == 1 {
            types.into_iter().next().unwrap()
        } else {
            Self::Union(types)
        }
    }

    pub fn intersection(types: Vec<MagType>) -> Self {
        if types.len() == 1 {
            types.into_iter().next().unwrap()
        } else {
            Self::Intersection(types)
        }
    }

    pub fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }

    pub fn variants(&self) -> Vec<&MagType> {
        match self {
            Self::Union(types) => types.iter().collect(),
            other => vec![other],
        }
    }

    pub fn accepts(&self, other: &MagType) -> bool {
        match (self, other) {
            (Self::Named(a), Self::Named(b)) => a == b,
            (Self::Var(_), _) | (_, Self::Var(_)) => true,
            (Self::Union(variants), ty) => variants.iter().any(|v| v.accepts(ty)),
            (ty, Self::Union(variants)) => variants.iter().all(|v| ty.accepts(v)),
            (Self::Intersection(required), ty) => required.iter().all(|r| r.accepts(ty)),
            (ty, Self::Intersection(provided)) => provided.iter().any(|p| ty.accepts(p)),
        }
    }

    pub fn is_variant_of(&self, union_type: &MagType) -> bool {
        match union_type {
            MagType::Union(variants) => variants.iter().any(|v| v.accepts(self)),
            _ => union_type.accepts(self),
        }
    }
}

impl fmt::Display for MagType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => write!(f, "{name}"),
            Self::Var(name) => write!(f, "{name}"),
            Self::Union(types) => {
                let parts: Vec<_> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "({})", parts.join(" | "))
            }
            Self::Intersection(types) => {
                let parts: Vec<_> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "({})", parts.join(" + "))
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Substitution(HashMap<String, MagType>);

impl Substitution {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn bind(&mut self, var: &str, ty: MagType) -> Result<(), String> {
        if let Some(existing) = self.0.get(var) {
            if existing != &ty {
                return Err(format!(
                    "type variable {var} already bound to {existing}, cannot unify with {ty}"
                ));
            }
        } else {
            self.0.insert(var.to_string(), ty);
        }
        Ok(())
    }

    pub fn apply(&self, ty: &MagType) -> MagType {
        match ty {
            MagType::Var(name) => self.0.get(name).cloned().unwrap_or_else(|| ty.clone()),
            MagType::Union(types) => MagType::Union(types.iter().map(|t| self.apply(t)).collect()),
            MagType::Intersection(types) => {
                MagType::Intersection(types.iter().map(|t| self.apply(t)).collect())
            }
            MagType::Named(_) => ty.clone(),
        }
    }
}

pub fn unify(a: &MagType, b: &MagType, subst: &mut Substitution) -> Result<(), String> {
    match (a, b) {
        (MagType::Var(name), ty) | (ty, MagType::Var(name)) => subst.bind(name, ty.clone()),
        (MagType::Named(a), MagType::Named(b)) if a == b => Ok(()),
        (MagType::Named(a), MagType::Named(b)) => Err(format!("type mismatch: {a} vs {b}")),
        (MagType::Union(a_variants), MagType::Union(b_variants))
            if a_variants.len() == b_variants.len() =>
        {
            for (a, b) in a_variants.iter().zip(b_variants.iter()) {
                unify(a, b, subst)?;
            }
            Ok(())
        }
        (MagType::Intersection(a_parts), MagType::Intersection(b_parts))
            if a_parts.len() == b_parts.len() =>
        {
            for (a, b) in a_parts.iter().zip(b_parts.iter()) {
                unify(a, b, subst)?;
            }
            Ok(())
        }
        _ => Err(format!("cannot unify {a} with {b}")),
    }
}
