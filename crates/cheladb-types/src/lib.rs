//! Shared domain identifiers for Cheladb control plane.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype!(TenantId, "Stable identifier for a tenant (organization).");
id_newtype!(ProjectId, "Stable identifier for a project within a tenant.");
id_newtype!(DatabaseId, "Stable identifier for a provisioned database instance.");
id_newtype!(
    BranchId,
    "Stable identifier for a branch of a database; main's equals its database id."
);

/// A database's main branch: its id is the database's.
impl From<DatabaseId> for BranchId {
    fn from(value: DatabaseId) -> Self {
        Self(value.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_display() {
        let id = DatabaseId::new();
        let parsed: Uuid = id.to_string().parse().unwrap();
        assert_eq!(id.as_uuid(), parsed);
    }

    #[test]
    fn main_branch_id_is_the_database_id() {
        let db = DatabaseId::new();
        assert_eq!(BranchId::from(db).as_uuid(), db.as_uuid());
    }
}
pub mod mode;
