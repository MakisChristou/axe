use std::fmt;
use std::ops::Deref;

macro_rules! chain_name {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub(crate) struct $name(String);

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

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
    };
}

macro_rules! validated_string {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: String) -> eyre::Result<Self> {
                if value.trim().is_empty() {
                    return Err(eyre::eyre!(concat!(stringify!($name), " cannot be empty")));
                }
                Ok(Self(value))
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

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
    };
}

chain_name!(
    DisplayChainName,
    "Human-facing chain name used in CLI output and reports."
);
validated_string!(
    AxelarChainId,
    "Axelar-side chain identifier used for routing and contract queries."
);
validated_string!(
    ConfigChainId,
    "Non-empty key into the chains-config `chains` map; resolution also checks that it exists."
);

/// Parsed RPC endpoint kept distinct from chain and address strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RpcUrl(String);

impl RpcUrl {
    pub(crate) fn new(value: String) -> eyre::Result<Self> {
        let parsed =
            reqwest::Url::parse(&value).map_err(|error| eyre::eyre!("invalid RPC URL: {error}"))?;
        match parsed.scheme() {
            "http" | "https" | "ws" | "wss" => Ok(Self(value)),
            scheme => Err(eyre::eyre!(
                "invalid RPC URL scheme '{scheme}'; expected http, https, ws, or wss"
            )),
        }
    }
}

impl AsRef<str> for RpcUrl {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Deref for RpcUrl {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for RpcUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl PartialEq<&str> for RpcUrl {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl DisplayChainName {
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{AxelarChainId, ConfigChainId, RpcUrl};

    #[test]
    fn semantic_boundary_strings_reject_empty_values() {
        assert!(ConfigChainId::new(String::new()).is_err());
        assert!(AxelarChainId::new("  ".to_string()).is_err());
        assert!(RpcUrl::new("\t".to_string()).is_err());
        assert!(RpcUrl::new("file:///tmp/not-an-rpc".to_string()).is_err());
    }

    #[test]
    fn semantic_boundary_strings_preserve_external_values() {
        let chain = ConfigChainId::new("flow".to_string()).expect("valid chain id");
        let axelar = AxelarChainId::new("Flow".to_string()).expect("valid Axelar id");
        let rpc = RpcUrl::new("https://rpc.example".to_string()).expect("valid RPC");

        assert_eq!(chain.as_ref(), "flow");
        assert_eq!(axelar.as_ref(), "Flow");
        assert_eq!(rpc.as_ref(), "https://rpc.example");
    }
}
