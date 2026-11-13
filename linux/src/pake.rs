//! The password-authenticated key exchange behind linking (PROTOCOL.md, Linking): CPace over
//! X25519, with the short code the Mac shows as the password and both TLS certificates'
//! fingerprints (as each side saw them in the handshake) bound in.
//!
//! Both sides derive a generator point from the code and the fingerprints, each sends a share
//! (its random scalar times that point), and each proves it got the same key with a tag. One run
//! lets someone who doesn't know the code test one guess, and only by talking to the other side;
//! nothing seen on the wire, or by a fake Mac or fake Linux, lets it test guesses offline. A
//! computer relaying the exchange in between holds its own certificate on each side, so the two
//! sides derive different generators and the tags don't match: it counts as a wrong guess.
//!
//! The Mac's `LinkPake` (mac/Sources/SweeflowHost/LinkPake.swift) is the same sum, step for step:
//! the tests on both sides check the same vectors.
//!
//! The generator: `r` = SHA-256(label ‖ code ‖ fingerprints) with its top bit cleared, as a field
//! element; G = the u-coordinate of Elligator 2 (RFC 9380 §6.7.1, Z = 2) of `r` on Curve25519.
//! Elligator 2 always lands on the curve itself, never on its twist, so the shares (clamped
//! scalars times G) are points of the curve's prime-order subgroup whatever the code: they say
//! nothing about it. (Hashing straight to a u-coordinate would land on the twist for about half
//! the codes, and a share's being on the curve or the twist would then tell an onlooker which half
//! the code is in.) Nobody knows the discrete log of G to the base of another code's G, which is
//! what keeps one run to one guess.
//!
//! A share received may be anything, also a point of the twist or of small order: X25519's
//! clamping makes every scalar a multiple of 8, which clears the small-order part (both groups'
//! cofactors are 8 and 4), and Curve25519's twist has a large prime-order subgroup too (it's
//! "twist-secure"), so a twist point gives a stranger nothing to work with. A key that comes out
//! as all zeroes (a small-order share) is refused.

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

/// Names this exchange in every hash, so its values can't be mistaken for any other's.
const LABEL: &[u8] = b"sweeflow-link/3";

/// Field elements mod p = 2^255 − 19, as four little-endian 64-bit limbs, always fully reduced.
/// Only for the generator (public operations on the code's hash), so plain arithmetic is fine.
type Fe = [u64; 4];

const P: Fe = [0xffff_ffff_ffff_ffed, u64::MAX, u64::MAX, 0x7fff_ffff_ffff_ffff];
/// p − 2 (inversion) and (p − 1) / 2 (the Legendre symbol).
const P_MINUS_2: Fe = [0xffff_ffff_ffff_ffeb, u64::MAX, u64::MAX, 0x7fff_ffff_ffff_ffff];
const HALF_P: Fe = [0xffff_ffff_ffff_fff6, u64::MAX, u64::MAX, 0x3fff_ffff_ffff_ffff];
const ONE: Fe = [1, 0, 0, 0];
/// Curve25519's A.
const A: Fe = [486_662, 0, 0, 0];

fn geq(a: &Fe, b: &Fe) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// a − b mod 2^256, and whether it borrowed.
fn sub_raw(a: &Fe, b: &Fe) -> (Fe, bool) {
    let mut out = [0u64; 4];
    let mut borrow = false;
    for i in 0..4 {
        let (d, b1) = a[i].overflowing_sub(b[i]);
        let (d, b2) = d.overflowing_sub(borrow as u64);
        out[i] = d;
        borrow = b1 || b2;
    }
    (out, borrow)
}

/// a + b mod 2^256, and whether it carried.
fn add_raw(a: &Fe, b: &Fe) -> (Fe, bool) {
    let mut out = [0u64; 4];
    let mut carry = false;
    for i in 0..4 {
        let (s, c1) = a[i].overflowing_add(b[i]);
        let (s, c2) = s.overflowing_add(carry as u64);
        out[i] = s;
        carry = c1 || c2;
    }
    (out, carry)
}

/// Any 256-bit value, reduced mod p (it's below 2p + 38, so at most a few subtractions).
fn reduce(mut a: Fe) -> Fe {
    while geq(&a, &P) {
        a = sub_raw(&a, &P).0;
    }
    a
}

fn add(a: &Fe, b: &Fe) -> Fe {
    // Both below p, so the sum is below 2^256.
    reduce(add_raw(a, b).0)
}

fn sub(a: &Fe, b: &Fe) -> Fe {
    let (d, borrow) = sub_raw(a, b);
    // a − b + 2^256 when it borrowed; adding p and dropping the carry gives a − b + p.
    if borrow { add_raw(&d, &P).0 } else { d }
}

fn mul(a: &Fe, b: &Fe) -> Fe {
    let mut t = [0u128; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let v = t[i + j] + (a[i] as u128) * (b[j] as u128) + carry;
            t[i + j] = v & u64::MAX as u128;
            carry = v >> 64;
        }
        t[i + 4] = carry;
    }
    // 2^256 ≡ 38 (mod p): fold the top half onto the bottom one.
    let mut r = [0u64; 4];
    let mut carry = 0u128;
    for i in 0..4 {
        let v = t[i] + 38 * t[i + 4] + carry;
        r[i] = v as u64;
        carry = v >> 64;
    }
    // What's left over is below 2^7: fold it once more (it may carry out one last time).
    let (r2, c) = add_raw(&r, &[38 * carry as u64, 0, 0, 0]);
    let r3 = if c { add_raw(&r2, &[38, 0, 0, 0]).0 } else { r2 };
    reduce(r3)
}

fn pow(a: &Fe, e: &Fe) -> Fe {
    let mut r = ONE;
    for i in (0..4).rev() {
        for bit in (0..64).rev() {
            r = mul(&r, &r);
            if (e[i] >> bit) & 1 == 1 {
                r = mul(&r, a);
            }
        }
    }
    r
}

fn from_bytes(b: &[u8; 32]) -> Fe {
    let mut f = [0u64; 4];
    for (i, limb) in f.iter_mut().enumerate() {
        *limb = u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().expect("8 bytes"));
    }
    f
}

fn to_bytes(f: &Fe) -> [u8; 32] {
    let mut b = [0u8; 32];
    for i in 0..4 {
        b[8 * i..8 * i + 8].copy_from_slice(&f[i].to_le_bytes());
    }
    b
}

/// v is a square mod p (zero counts as one).
fn is_square(v: &Fe) -> bool {
    let l = pow(v, &HALF_P);
    l == ONE || l == [0; 4]
}

/// Elligator 2 onto Curve25519 (RFC 9380 §6.7.1, Z = 2), the u-coordinate only.
fn elligator2(r: &Fe) -> Fe {
    let r2 = mul(r, r);
    let tv = add(&ONE, &add(&r2, &r2));
    // inv0: the inverse, or 0 for 0.
    let inv = pow(&tv, &P_MINUS_2);
    let minus_a = sub(&[0; 4], &A);
    let mut x1 = mul(&minus_a, &inv);
    if x1 == [0; 4] {
        x1 = minus_a;
    }
    // g(x1) = x1³ + A·x1² + x1 = x1·((x1 + A)·x1 + 1)
    let gx1 = mul(&x1, &add(&mul(&add(&x1, &A), &x1), &ONE));
    if is_square(&gx1) { x1 } else { sub(&minus_a, &x1) }
}

/// SHA-256 over these parts, each preceded by its length (one byte: none is longer than 255).
fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update([p.len() as u8]);
        h.update(p);
    }
    h.finalize().into()
}

/// The fingerprint as it goes into the sums: 64 lowercase hex characters.
fn fp(f: &str) -> String {
    f.to_ascii_lowercase()
}

/// The generator for this code and these two certificates (each side uses the fingerprints as it
/// saw them: its own certificate's and the one the other side proved it holds).
pub fn generator(code: &str, client_fingerprint: &str, host_fingerprint: &str) -> [u8; 32] {
    let (c, h) = (fp(client_fingerprint), fp(host_fingerprint));
    let mut r = hash(&[LABEL, b"generator", code.as_bytes(), c.as_bytes(), h.as_bytes()]);
    r[31] &= 0x7f;
    to_bytes(&elligator2(&reduce(from_bytes(&r))))
}

/// One side's half of a run: its scalar (secret) and its share.
pub struct Share {
    scalar: [u8; 32],
    pub public: [u8; 32],
}

impl Share {
    pub fn new(generator: &[u8; 32], scalar: [u8; 32]) -> Share {
        Share { scalar, public: x25519_dalek::x25519(scalar, *generator) }
    }

    /// A share with a fresh random scalar.
    pub fn random(generator: &[u8; 32]) -> Result<Share> {
        let mut s = [0u8; 32];
        rustls::crypto::ring::default_provider().secure_random.fill(&mut s).map_err(|_| anyhow::anyhow!("no random numbers"))?;
        Ok(Share::new(generator, s))
    }
}

/// What a finished run gives both sides when they used the same code: the tag each sends to show
/// it has the key.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Tags {
    pub client: [u8; 32],
    pub host: [u8; 32],
}

/// The run's key and tags. `ours` is this side's share, `theirs` the other's; `client_share` and
/// `host_share` are the two in the order of the roles, whoever computes it.
fn finish(ours: &Share, theirs: &[u8; 32], client_fingerprint: &str, host_fingerprint: &str, client_share: &[u8; 32], host_share: &[u8; 32]) -> Result<Tags> {
    let k = x25519_dalek::x25519(ours.scalar, *theirs);
    if k == [0; 32] {
        bail!("the other side's share is of small order");
    }
    let (c, h) = (fp(client_fingerprint), fp(host_fingerprint));
    let key = hash(&[LABEL, b"key", c.as_bytes(), h.as_bytes(), client_share, host_share, &k]);
    let tag = |role: &[u8]| -> [u8; 32] {
        let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key);
        ring::hmac::sign(&k, &[LABEL, role].concat()).as_ref().try_into().expect("32 bytes")
    };
    Ok(Tags { client: tag(b"client"), host: tag(b"host") })
}

/// The client's (Linux's) view of a run.
pub fn client_tags(ours: &Share, host_share: &[u8; 32], client_fingerprint: &str, host_fingerprint: &str) -> Result<Tags> {
    finish(ours, host_share, client_fingerprint, host_fingerprint, &ours.public, host_share)
}

/// The host's (Mac's) view of a run (for tests here; the Mac has its own).
#[cfg(test)]
pub fn host_tags(ours: &Share, client_share: &[u8; 32], client_fingerprint: &str, host_fingerprint: &str) -> Result<Tags> {
    finish(ours, client_share, client_fingerprint, host_fingerprint, client_share, &ours.public)
}

/// Compares tags without stopping at the first difference.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const CLIENT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    pub const HOST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn curve_rhs(u: &Fe) -> Fe {
        mul(u, &add(&mul(&add(u, &A), u), &ONE))
    }

    #[test]
    fn field_arithmetic_holds_up() {
        let minus_one = sub(&[0; 4], &ONE);
        assert_eq!(minus_one, [0xffff_ffff_ffff_ffec, u64::MAX, u64::MAX, 0x7fff_ffff_ffff_ffff]);
        assert_eq!(mul(&minus_one, &minus_one), ONE);
        assert_eq!(add(&minus_one, &ONE), [0; 4]);
        let x = reduce(from_bytes(&hash(&[b"x"])));
        assert_eq!(mul(&x, &pow(&x, &P_MINUS_2)), ONE);
        // p ≡ 5 (mod 8): −1 is a square, 2 isn't.
        assert!(is_square(&minus_one));
        assert!(!is_square(&[2, 0, 0, 0]));
        assert!(is_square(&mul(&x, &x)));
        // 2^255 − 19 + 5 reads as 5.
        let mut over = P;
        over[0] += 5;
        assert_eq!(reduce(over), [5, 0, 0, 0]);
    }

    #[test]
    fn generators_are_on_the_curve_not_its_twist() {
        for i in 0..200u32 {
            let g = generator(&format!("{i:04}"), CLIENT, HOST);
            assert!(is_square(&curve_rhs(&from_bytes(&g))), "code {i:04}");
            assert!(g[31] & 0x80 == 0);
        }
        // r = 0 (1 + 2r² = 1, so x1 = −A) still gives a curve point.
        assert!(is_square(&curve_rhs(&elligator2(&[0; 4]))));
    }

    #[test]
    fn the_same_code_gives_both_sides_the_same_tags() {
        let g = generator("4821", CLIENT, HOST);
        let c = Share::random(&g).unwrap();
        let h = Share::random(&g).unwrap();
        let a = client_tags(&c, &h.public, CLIENT, HOST).unwrap();
        let b = host_tags(&h, &c.public, CLIENT, HOST).unwrap();
        assert_eq!(a, b);
        assert_ne!(a.client, a.host);
    }

    #[test]
    fn a_wrong_code_or_another_certificate_gives_other_tags() {
        let c = Share::random(&generator("4821", CLIENT, HOST)).unwrap();
        let h = Share::random(&generator("4822", CLIENT, HOST)).unwrap();
        assert_ne!(client_tags(&c, &h.public, CLIENT, HOST).unwrap(), host_tags(&h, &c.public, CLIENT, HOST).unwrap());
        // Someone in between: Linux sees its certificate where the Mac's should be.
        let other = "f".repeat(64);
        let c = Share::random(&generator("4821", CLIENT, &other)).unwrap();
        let h = Share::random(&generator("4821", CLIENT, HOST)).unwrap();
        assert_ne!(client_tags(&c, &h.public, CLIENT, &other).unwrap().client, host_tags(&h, &c.public, CLIENT, HOST).unwrap().client);
    }

    #[test]
    fn a_small_order_share_is_refused() {
        let c = Share::random(&generator("4821", CLIENT, HOST)).unwrap();
        assert!(client_tags(&c, &[0; 32], CLIENT, HOST).is_err());
        let mut one = [0u8; 32];
        one[0] = 1;
        assert!(client_tags(&c, &one, CLIENT, HOST).is_err());
    }

    /// The same inputs and outputs as the Mac's LinkPakeTests (mac/Tests/SweeflowHostTests), also
    /// checked against a plain big-integer version of the sums.
    #[test]
    fn vectors_match_the_mac() {
        let g = generator("4821", CLIENT, HOST);
        assert_eq!(hex::encode(g), VECTOR_G);
        let c = Share::new(&g, [0x11; 32]);
        let h = Share::new(&g, [0x22; 32]);
        assert_eq!(hex::encode(c.public), VECTOR_CLIENT_SHARE);
        assert_eq!(hex::encode(h.public), VECTOR_HOST_SHARE);
        let t = client_tags(&c, &h.public, CLIENT, HOST).unwrap();
        assert_eq!(hex::encode(t.client), VECTOR_CLIENT_TAG);
        assert_eq!(hex::encode(t.host), VECTOR_HOST_TAG);
        assert_eq!(hex::encode(generator("0000", &"0".repeat(64), &"F".repeat(64))), VECTOR_G_ZEROES);
    }

    const VECTOR_G: &str = "f38495f3d09d4c6de9368bd1637adfc8d604414c2462299affc4dbc3a9da9164";
    const VECTOR_CLIENT_SHARE: &str = "d410d7e9ee08dc0385033158aa11ced69db475ce190e7a927d1af8b9791e457d";
    const VECTOR_HOST_SHARE: &str = "0d2f93760c8fe9169de24088de63a4e3ea94fa7c6e8b6a2b2dc0f4e70b345a27";
    const VECTOR_CLIENT_TAG: &str = "6b71d09d0bae6b969a1e5538e41e86f7c26fe534f167a16d58333b6791a51de7";
    const VECTOR_HOST_TAG: &str = "0b627cbbf9957b7d2a350324ceb4d185fd6837222f6b5cf0812f54f98f452c47";
    const VECTOR_G_ZEROES: &str = "ab0bed9d25f73ed1252cd59249bbdebfb7e3ddaadd9f6bb1d2f07970dfb89a4f";
}
