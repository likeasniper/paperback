/*
 * paperback: paper backup generator suitable for long-term storage
 * Copyright (C) 2018-2020 Aleksa Sarai <cyphar@cyphar.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use crate::{
    shamir::gf::{GfElem, GfElemPrimitive, GfPolynomial},
    v0::{FromWire, ToWire},
};

use std::mem;

use rand::rngs::OsRng;
use unsigned_varint::encode;

/// Piece of a secret which has been sharded with [Shamir Secret Sharing][sss].
///
/// [sss]: https://en.wikipedia.org/wiki/Shamir%27s_Secret_Sharing
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shard {
    x: GfElem,
    ys: Vec<GfElem>,
    secret_len: usize,
    threshold: GfElemPrimitive,
}

impl Shard {
    pub const ID_LENGTH: usize = 8;

    /// Returns the *unique* identifier for a given `Shard`.
    ///
    /// If two shards have the same identifier, they cannot be used together for
    /// secret recovery.
    pub fn id(&self) -> String {
        let id = zbase32::encode_full_bytes(&self.x.to_bytes());
        format!("h{}", id)
    }

    /// Returns the number of *unique* sister `Shard`s required to recover the
    /// stored secret.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }
}

impl ToWire for Shard {
    fn to_wire(&self) -> Vec<u8> {
        let mut bytes = vec![];

        // Encode x-value.
        encode::u32(self.x.inner(), &mut encode::u32_buffer())
            .iter()
            .for_each(|b| bytes.push(*b));

        // Encode y-values (length-prefixed).
        encode::usize(self.ys.len(), &mut encode::usize_buffer())
            .iter()
            .copied()
            .chain(
                self.ys
                    .iter()
                    .flat_map(|y| encode::u32(y.inner(), &mut encode::u32_buffer()).to_owned()),
            )
            .for_each(|b| bytes.push(b));

        // Encode threshold.
        encode::u32(self.threshold, &mut encode::u32_buffer())
            .iter()
            .for_each(|b| bytes.push(*b));

        // Encode secret length.
        encode::usize(self.secret_len, &mut encode::usize_buffer())
            .iter()
            .for_each(|b| bytes.push(*b));

        bytes
    }
}

impl FromWire for Shard {
    fn from_wire_partial(input: &[u8]) -> Result<(Self, &[u8]), String> {
        use crate::nom_helpers;
        use nom::{combinator::complete, multi::many_m_n, IResult};

        fn parse(input: &[u8]) -> IResult<&[u8], Shard> {
            let (input, x) = nom_helpers::u32(input)?;
            let x = GfElem::from_inner(x);

            let (input, ys_length) = nom_helpers::usize(input)?;
            let (input, ys) = many_m_n(ys_length, ys_length, nom_helpers::u32)(input)?;
            let ys = ys
                .iter()
                .copied()
                .map(GfElem::from_inner)
                .collect::<Vec<_>>();

            let (input, threshold) = nom_helpers::u32(input)?;
            let (input, secret_len) = nom_helpers::usize(input)?;

            Ok((
                input,
                Shard {
                    x,
                    ys,
                    threshold,
                    secret_len,
                },
            ))
        }
        let parse = complete(parse);

        let (remain, shard) = parse(input).map_err(|err| format!("{:?}", err))?;

        Ok((shard, remain))
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Shard {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Self {
        Self {
            x: GfElem::new_rand(g),
            ys: (0..g.size()).map(|_| GfElem::new_rand(g)).collect(),
            secret_len: g.next_u32() as usize,
            threshold: g.next_u32(),
        }
    }
}

/// Factory to share a secret using [Shamir Secret Sharing][sss].
///
/// [sss]: https://en.wikipedia.org/wiki/Shamir%27s_Secret_Sharing
#[derive(Clone, Debug)]
pub struct Dealer {
    polys: Vec<GfPolynomial>,
    secret_len: usize,
    threshold: GfElemPrimitive,
}

impl Dealer {
    /// Returns the number of *unique* `Shard`s generated by this `Dealer`
    /// required to recover the stored secret.
    #[allow(dead_code)]
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Construct a new `Dealer` to shard the `secret`, requiring at least
    /// `threshold` shards to reconstruct the secret.
    pub fn new<B: AsRef<[u8]>>(threshold: u32, secret: B) -> Self {
        assert!(threshold > 0, "must at least have a threshold of one");
        let k = threshold - 1;
        let secret = secret.as_ref();
        let polys = secret
            // Generate &[u32] from &[u8], by chunking into sets of four.
            .chunks(mem::size_of::<GfElemPrimitive>())
            .map(GfElem::from_bytes)
            // Generate a random polynomial with the value as the constant.
            .map(|x0| {
                let mut poly = GfPolynomial::new_rand(k, &mut OsRng);
                *poly.constant_mut() = x0;
                poly
            })
            .collect::<Vec<_>>();
        Dealer {
            polys,
            threshold,
            secret_len: secret.len(),
        }
    }

    /// Get the secret value stored by the `Dealer`.
    pub fn secret(&self) -> Vec<u8> {
        self.polys
            .iter()
            .map(GfPolynomial::constant)
            .flat_map(|x| x.to_bytes())
            .take(self.secret_len)
            .collect::<Vec<_>>()
    }

    /// Generate a new `Shard` for the secret.
    ///
    /// NOTE: The `x` value is calculated randomly, which means that there is a
    ///       small chance that two separate calls to `Dealer::shard` will
    ///       generate the same `Shard`. It is up to the caller to be sure that
    ///       they have enough *unique* shards to reconstruct the secret.
    // TODO: I'm not convinced the chances of collision are low enough...
    pub fn next_shard(&self) -> Shard {
        let mut x = GfElem::ZERO;
        while x == GfElem::ZERO {
            x = GfElem::new_rand(&mut OsRng);
        }
        let ys = self
            .polys
            .iter()
            .map(|poly| {
                let y = poly.evaluate(x);
                assert!(self.threshold == 1 || y != poly.constant());
                y
            })
            .collect::<Vec<_>>();
        Shard {
            x,
            ys,
            threshold: self.threshold,
            secret_len: self.secret_len,
        }
    }

    /// Reconstruct an entire `Dealer` from a *unique* set of `Shard`s.
    ///
    /// The caller must pass exactly the correct number of shards.
    ///
    /// This operation is significantly slower than `recover_secret`, so it
    /// should only be used if it is necessary to construct additional shards
    /// with `Dealer::next_shard`.
    pub fn recover<S: AsRef<[Shard]>>(shards: S) -> Self {
        // TODO: Add -> Result<Self, _>.
        let shards = shards.as_ref();
        assert!(shards.len() > 0, "must be provided at least one shard");

        let threshold = shards[0].threshold;
        let polys_len = shards[0].ys.len();
        let secret_len = shards[0].secret_len;

        // TODO: Implement this consistency checking more nicely.
        for shard in shards {
            assert!(shard.threshold == threshold, "shards must be consistent");
            assert!(shard.ys.len() == polys_len, "shards must be consistent");
            assert!(shard.secret_len == secret_len, "shards must be consistent");
        }

        assert!(
            shards.len() == threshold as usize,
            "must have exactly {} shards",
            threshold
        );

        let polys = (0..polys_len)
            .map(|i| {
                let xs = shards.iter().map(|s| s.x);
                let ys = shards.iter().map(|s| s.ys[i]);

                let points = xs.zip(ys).collect::<Vec<_>>();
                GfPolynomial::lagrange(threshold - 1, points.as_slice())
            })
            .collect::<Vec<_>>();

        Self {
            polys,
            threshold,
            secret_len,
        }
    }
}

/// Reconstruct a secret from a set of `Shard`s.
///
/// This operation is significantly faster than `Dealer::recover`, so it should
/// always be used if the caller only needs to recover the secret.
/// `Dealer::recover` should only be used if the caller needs to create
/// additional shards with `Dealer::next_shard`.
pub fn recover_secret<S: AsRef<[Shard]>>(shards: S) -> Vec<u8> {
    // TODO: Add -> Result<Vec<u8>, _>.
    let shards = shards.as_ref();
    assert!(shards.len() > 0, "must be provided at least one shard");

    let threshold = shards[0].threshold;
    let polys_len = shards[0].ys.len();
    let secret_len = shards[0].secret_len;

    // TODO: Implement this consistency checking more nicely.
    for shard in shards {
        assert!(shard.threshold == threshold, "shards must be consistent");
        assert!(shard.ys.len() == polys_len, "shards must be consistent");
        assert!(shard.secret_len == secret_len, "shards must be consistent");
    }

    assert!(
        shards.len() == threshold as usize,
        "must have exactly {} shards",
        threshold
    );

    (0..polys_len)
        .map(|i| {
            let xs = shards.iter().map(|s| s.x);
            let ys = shards.iter().map(|s| s.ys[i]);

            let points = xs.zip(ys).collect::<Vec<_>>();
            GfPolynomial::lagrange_constant(threshold - 1, points.as_slice())
        })
        .flat_map(|x| x.to_bytes())
        .take(secret_len)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod test {
    use super::*;

    use quickcheck::TestResult;

    #[quickcheck]
    fn basic_roundtrip(n: u32, secret: Vec<u8>) -> TestResult {
        if n < 1 {
            return TestResult::discard();
        }
        let dealer = Dealer::new(n, &secret);
        TestResult::from_bool(secret == dealer.secret())
    }

    #[quickcheck]
    fn shard_bytes_roundtrip(shard: Shard) {
        let shard2 = Shard::from_wire(&shard.to_wire()).unwrap();
        assert_eq!(shard, shard2);
    }

    #[quickcheck]
    fn recover_secret_fail(n: u32, secret: Vec<u8>) -> TestResult {
        // Invalid data. Note that large n values take a very long time to
        // recover the secret. This is proportional to secret.len(), which is
        // controlled by quickcheck and thus can be quite large.
        if n < 2 || n > 32 || secret.len() < 1 {
            return TestResult::discard();
        }

        let dealer = Dealer::new(n, &secret);
        let shards = (0..(n - 1))
            .map(|_| {
                let mut shard = dealer.next_shard();
                shard.threshold -= 1;
                // Ensure shard IDs are always ID_LENGTH.
                assert_eq!(shard.id().len(), Shard::ID_LENGTH);
                shard
            })
            .collect::<Vec<_>>();

        TestResult::from_bool(recover_secret(shards) != secret)
    }

    #[quickcheck]
    fn recover_secret_success(n: u32, secret: Vec<u8>) -> TestResult {
        // Invalid data. Note that large n values take a very long time to
        // recover the secret. This is proportional to secret.len(), which is
        // controlled by quickcheck and thus can be quite large.
        if n < 1 || n > 32 {
            return TestResult::discard();
        }

        let dealer = Dealer::new(n, &secret);
        let shards = (0..n)
            .map(|_| {
                let shard = dealer.next_shard();
                // Ensure shard IDs are always ID_LENGTH.
                assert_eq!(shard.id().len(), Shard::ID_LENGTH);
                shard
            })
            .collect::<Vec<_>>();

        TestResult::from_bool(recover_secret(shards) == secret)
    }

    #[quickcheck]
    fn recover_success(n: u32, secret: Vec<u8>) -> TestResult {
        // Invalid data. Note that even moderately large n values take a very
        // long time to fully recover. This is proportional to secret.len().
        if n < 2 || n > 8 {
            return TestResult::discard();
        }

        let dealer = Dealer::new(n, secret);
        let shards = (0..n)
            .map(|_| {
                let shard = dealer.next_shard();
                // Ensure shard IDs are always ID_LENGTH.
                assert_eq!(shard.id().len(), Shard::ID_LENGTH);
                shard
            })
            .collect::<Vec<_>>();
        let recovered_dealer = Dealer::recover(shards);

        TestResult::from_bool(dealer.polys == recovered_dealer.polys)
    }
}