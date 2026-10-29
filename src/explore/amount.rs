//! [`Amount`]: an amount relative to a balance read when the op submits, so
//! a plan stays meaningful whatever the balance is then; [`any`] draws one.

use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt;

/// An amount relative to a balance: all of it, a fraction, one more than
/// all of it, an exact number. An op reads the balance when it submits and
/// resolves the amount with [`of`](Self::of); `?` turns an [`AmountError`]
/// into a [`Skip`](super::Skip).
///
/// ```
/// use near_contract_sim::explore::Amount;
/// assert_eq!(Amount::All.of(40), Ok(40));
/// assert_eq!(Amount::AllPlus(1).of(40), Ok(41)); // an overdraw, on purpose
/// assert_eq!(Amount::Fraction(1, 4).of(40), Ok(10));
/// assert!(Amount::AllMinus(50).of(40).is_err());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Amount {
    /// 0.
    Zero,
    /// 1.
    One,
    /// The whole balance.
    All,
    /// The balance plus this much: an overdraw.
    AllPlus(u128),
    /// The balance less this much; an error if the balance is smaller.
    AllMinus(u128),
    /// `numerator / denominator` of the balance, rounded down; an error if
    /// the denominator is 0.
    Fraction(u32, u32),
    /// Exactly this, whatever the balance.
    Exact(u128),
}

impl Amount {
    /// The amount for `balance`, or why it has none: the balance is below
    /// what [`AllMinus`](Self::AllMinus) takes off, [`AllPlus`](Self::AllPlus)
    /// overflows, or a [`Fraction`](Self::Fraction) has denominator 0.
    pub fn of(self, balance: u128) -> Result<u128, AmountError> {
        let err = || AmountError::new(self, balance);
        match self {
            Amount::Zero => Ok(0),
            Amount::One => Ok(1),
            Amount::All => Ok(balance),
            Amount::AllPlus(n) => balance.checked_add(n).ok_or_else(err),
            Amount::AllMinus(n) => balance.checked_sub(n).ok_or_else(err),
            Amount::Fraction(_, 0) => Err(err()),
            Amount::Fraction(n, d) => {
                let (n, d) = (u128::from(n), u128::from(d));
                // balance * n / d without overflowing.
                let whole = (balance / d).checked_mul(n).ok_or_else(err)?;
                whole.checked_add(balance % d * n / d).ok_or_else(err)
            }
            Amount::Exact(n) => Ok(n),
        }
    }
}

/// An [`Amount`] that has no value for a balance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmountError {
    amount: Amount,
    balance: u128,
}

impl AmountError {
    pub(crate) fn new(amount: Amount, balance: u128) -> Self {
        AmountError { amount, balance }
    }
}

/// `AllMinus(5) of a balance of 3`.
impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} of a balance of {}", self.amount, self.balance)
    }
}

impl std::error::Error for AmountError {}

/// Any [`Amount`], weighted toward the edges a bug hides at: all, a fraction,
/// one, zero, a small overdraw, all but a little, exact values up to
/// `u128::MAX`. More than all (an overdraw, or `u128::MAX`) in about a
/// quarter of draws. Shrinks toward [`Amount::All`].
pub fn any() -> impl Strategy<Value = Amount> + Clone {
    let little = 1u128..=10;
    prop_oneof![
        3 => Just(Amount::All),
        3 => (1u32..=3).prop_map(|n| Amount::Fraction(n, 4)),
        1 => Just(Amount::One),
        1 => Just(Amount::Zero),
        2 => little.clone().prop_map(Amount::AllPlus),
        1 => little.prop_map(Amount::AllMinus),
        1 => (1u128..=1000).prop_map(Amount::Exact),
        1 => Just(Amount::Exact(u128::MAX)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    #[test]
    fn amounts_resolve() {
        assert_eq!(Amount::Zero.of(9), Ok(0));
        assert_eq!(Amount::One.of(0), Ok(1));
        assert_eq!(Amount::All.of(9), Ok(9));
        assert_eq!(Amount::AllPlus(2).of(9), Ok(11));
        assert!(Amount::AllPlus(1).of(u128::MAX).is_err());
        assert_eq!(Amount::AllMinus(9).of(9), Ok(0));
        assert_eq!(
            Amount::AllMinus(10).of(9).unwrap_err().to_string(),
            "AllMinus(10) of a balance of 9"
        );
        assert_eq!(Amount::Fraction(3, 4).of(10), Ok(7));
        assert_eq!(Amount::Fraction(3, 4).of(u128::MAX), Ok(u128::MAX / 4 * 3 + 2));
        assert!(Amount::Fraction(1, 0).of(10).is_err());
        assert_eq!(Amount::Fraction(5, 4).of(8), Ok(10));
        assert_eq!(Amount::Exact(u128::MAX).of(0), Ok(u128::MAX));
    }

    /// About a quarter of draws ask for more than all of it: 3 weights in 13.
    #[test]
    fn any_overdraws_in_about_a_quarter() {
        let mut runner = TestRunner::deterministic();
        let n = 10_000;
        let over = (0..n)
            .filter(|_| {
                any().new_tree(&mut runner).unwrap().current().of(1000).is_ok_and(|a| a > 1000)
            })
            .count();
        assert!((2_000..=2_600).contains(&over), "{over} of {n}");
    }

    #[test]
    fn any_shrinks_to_all() {
        let mut runner = TestRunner::deterministic();
        for _ in 0..50 {
            let mut tree = any().new_tree(&mut runner).unwrap();
            while tree.simplify() {}
            assert_eq!(tree.current(), Amount::All);
        }
    }
}
