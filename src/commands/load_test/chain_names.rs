use std::fmt;
use std::ops::Deref;

macro_rules! chain_name {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub(super) struct $name(String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

chain_name!(
    DisplayChainName,
    "Human-facing chain name used in CLI output and reports."
);
chain_name!(
    AxelarChainId,
    "Axelar-side chain identifier used for routing and contract queries."
);

impl DisplayChainName {
    pub(super) fn into_string(self) -> String {
        self.0
    }
}
