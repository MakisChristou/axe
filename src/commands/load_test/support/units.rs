//! Unit-safe load test amounts.
//!
//! RPC/client boundaries still receive their SDK-native integer types. Inside
//! the load-test layer these wrappers prevent, for example, passing lamports
//! where stroops or XRP drops are expected.

use alloy::primitives::U256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Wei(U256);

impl Wei {
    pub(super) const fn from_u128(value: u128) -> Self {
        Self(U256::from_limbs([value as u64, (value >> 64) as u64, 0, 0]))
    }

    pub(super) const fn from_u256(value: U256) -> Self {
        Self(value)
    }

    pub(super) const fn as_u256(self) -> U256 {
        self.0
    }

    pub(super) fn saturating_mul(self, rhs: u64) -> Self {
        Self(self.0.saturating_mul(U256::from(rhs)))
    }
}

macro_rules! u64_unit {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
        pub(super) struct $name(u64);

        impl $name {
            pub(super) const fn new(value: u64) -> Self {
                Self(value)
            }

            pub(super) const fn get(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

u64_unit!(Lamports);
u64_unit!(Stroops);
u64_unit!(Drops);
u64_unit!(Mist);

impl Drops {
    pub(super) const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

#[cfg(test)]
mod tests {
    use super::{Drops, Lamports, Mist, Stroops, Wei};
    use alloy::primitives::U256;

    #[test]
    fn unit_wrappers_preserve_values_and_arithmetic() {
        assert_eq!(
            Wei::from_u256(U256::from(4)).saturating_mul(3).as_u256(),
            U256::from(12)
        );
        assert_eq!(Lamports::new(4).get(), 4);
        assert_eq!(Stroops::new(4).get(), 4);
        assert_eq!(Mist::new(4).get(), 4);
        assert_eq!(
            Drops::new(u64::MAX).saturating_add(Drops::new(1)).get(),
            u64::MAX
        );
    }
}
