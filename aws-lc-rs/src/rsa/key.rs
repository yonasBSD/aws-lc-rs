// Copyright 2015-2016 Brian Smith.
// SPDX-License-Identifier: ISC
// Modifications copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR ISC

use super::signature::{compute_rsa_signature, RsaEncoding, RsaPadding};
use super::{encoding, RsaParameters};
#[cfg(feature = "fips")]
use crate::aws_lc::RSA_check_fips;
use crate::aws_lc::{
    EVP_DigestSignInit, EVP_PKEY_assign_RSA, EVP_PKEY_bits, EVP_PKEY_new, EVP_PKEY_size,
    RSA_generate_key_ex, RSA_generate_key_fips, RSA_new, RSA_set0_key, RSA_size, BIGNUM, EVP_PKEY,
    EVP_PKEY_CTX,
};
#[cfg(feature = "ring-io")]
use crate::aws_lc::{RSA_get0_e, RSA_get0_n};
use crate::digest::{self};
use crate::encoding::{AsDer, Pkcs8V1Der};
use crate::error::{KeyRejected, Unspecified};
use crate::fips::indicator_check;
#[cfg(feature = "ring-io")]
use crate::io;
#[cfg(feature = "ring-io")]
use crate::ptr::ConstPointer;
use crate::ptr::{DetachableLcPtr, LcPtr};
use crate::rsa::PublicEncryptingKey;
use crate::sealed::Sealed;
use crate::{hex, rand};
use core::fmt::{self, Debug, Formatter};
use core::ptr::null_mut;

// TODO: Uncomment when MSRV >= 1.64
// use core::ffi::c_int;
use std::os::raw::c_int;

#[cfg(feature = "ring-io")]
use untrusted::Input;
use zeroize::Zeroize;

/// RSA key-size.
#[allow(clippy::module_name_repetitions)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySize {
    /// 2048-bit key
    Rsa2048,

    /// 3072-bit key
    Rsa3072,

    /// 4096-bit key
    Rsa4096,

    /// 8192-bit key
    Rsa8192,
}

#[allow(clippy::len_without_is_empty)]
impl KeySize {
    /// Returns the size of the key in bytes.
    #[inline]
    #[must_use]
    pub fn len(self) -> usize {
        match self {
            Self::Rsa2048 => 256,
            Self::Rsa3072 => 384,
            Self::Rsa4096 => 512,
            Self::Rsa8192 => 1024,
        }
    }

    /// Returns the key size in bits.
    #[inline]
    pub(super) fn bits(self) -> i32 {
        match self {
            Self::Rsa2048 => 2048,
            Self::Rsa3072 => 3072,
            Self::Rsa4096 => 4096,
            Self::Rsa8192 => 8192,
        }
    }
}

/// An RSA key pair, used for signing.
#[allow(clippy::module_name_repetitions)]
pub struct KeyPair {
    // https://github.com/aws/aws-lc/blob/ebaa07a207fee02bd68fe8d65f6b624afbf29394/include/openssl/evp.h#L295
    // An |EVP_PKEY| object represents a public or private RSA key. A given object may be
    // used concurrently on multiple threads by non-mutating functions, provided no
    // other thread is concurrently calling a mutating function. Unless otherwise
    // documented, functions which take a |const| pointer are non-mutating and
    // functions which take a non-|const| pointer are mutating.
    pub(super) evp_pkey: LcPtr<EVP_PKEY>,
    pub(super) serialized_public_key: PublicKey,
}

impl Sealed for KeyPair {}
unsafe impl Send for KeyPair {}
unsafe impl Sync for KeyPair {}

impl KeyPair {
    fn new(evp_pkey: LcPtr<EVP_PKEY>) -> Result<Self, KeyRejected> {
        KeyPair::validate_private_key(&evp_pkey)?;
        let serialized_public_key = PublicKey::new(&evp_pkey)?;
        Ok(KeyPair {
            evp_pkey,
            serialized_public_key,
        })
    }

    /// Generate a RSA `KeyPair` of the specified key-strength.
    ///
    /// # Errors
    /// * `Unspecified`: Any key generation failure.
    pub fn generate(size: KeySize) -> Result<Self, Unspecified> {
        let private_key = generate_rsa_key(size.bits(), false)?;
        Ok(Self::new(private_key)?)
    }

    /// Generate a RSA `KeyPair` of the specified key-strength.
    ///
    /// Supports the following key sizes:
    /// * `SignatureKeySize::Rsa2048`
    /// * `SignatureKeySize::Rsa3072`
    /// * `SignatureKeySize::Rsa4096`
    ///
    /// # Errors
    /// * `Unspecified`: Any key generation failure.
    #[cfg(feature = "fips")]
    pub fn generate_fips(size: KeySize) -> Result<Self, Unspecified> {
        let private_key = generate_rsa_key(size.bits(), true)?;
        Ok(Self::new(private_key)?)
    }

    /// Parses an unencrypted PKCS#8 DER encoded RSA private key.
    ///
    /// Keys can be generated using [`KeyPair::generate`].
    ///
    /// # *ring*-compatibility
    ///
    /// *aws-lc-rs* does not impose the same limitations that *ring* does for
    /// RSA keys. Thus signatures may be generated by keys that are not accepted
    /// by *ring*. In particular:
    /// * RSA private keys ranging between 2048-bit keys and 8192-bit keys are supported.
    /// * The public exponent does not have a required minimum size.
    ///
    /// # Errors
    /// `error::KeyRejected` if bytes do not encode an RSA private key or if the key is otherwise
    /// not acceptable.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, KeyRejected> {
        let key = encoding::pkcs8::decode_der(pkcs8)?;
        Self::new(key)
    }

    /// Parses a DER-encoded `RSAPrivateKey` structure (RFC 8017).
    ///
    /// # Errors
    /// `error:KeyRejected` on error.
    pub fn from_der(input: &[u8]) -> Result<Self, KeyRejected> {
        let key = encoding::rfc8017::decode_private_key_der(input)?;
        Self::new(key)
    }

    /// Returns a boolean indicator if this RSA key is an approved FIPS 140-3 key.
    #[cfg(feature = "fips")]
    #[must_use]
    pub fn is_valid_fips_key(&self) -> bool {
        is_valid_fips_key(&self.evp_pkey)
    }

    fn validate_private_key(key: &LcPtr<EVP_PKEY>) -> Result<(), KeyRejected> {
        if !is_rsa_key(key) {
            return Err(KeyRejected::unspecified());
        };
        match key_size_bits(key) {
            2048..=8192 => Ok(()),
            _ => Err(KeyRejected::unspecified()),
        }
    }

    /// Sign `msg`. `msg` is digested using the digest algorithm from
    /// `padding_alg` and the digest is then padded using the padding algorithm
    /// from `padding_alg`. The signature it written into `signature`;
    /// `signature`'s length must be exactly the length returned by
    /// `public_modulus_len()`.
    ///
    /// Many other crypto libraries have signing functions that takes a
    /// precomputed digest as input, instead of the message to digest. This
    /// function does *not* take a precomputed digest; instead, `sign`
    /// calculates the digest itself.
    ///
    /// # *ring* Compatibility
    /// Our implementation ignores the `SecureRandom` parameter.
    // # FIPS
    // The following conditions must be met:
    // * RSA Key Sizes: 2048, 3072, 4096
    // * Digest Algorithms: SHA256, SHA384, SHA512
    //
    /// # Errors
    /// `error::Unspecified` on error.
    /// With "fips" feature enabled, errors if digest length is greater than `u32::MAX`.
    pub fn sign(
        &self,
        padding_alg: &'static dyn RsaEncoding,
        _rng: &dyn rand::SecureRandom,
        msg: &[u8],
        signature: &mut [u8],
    ) -> Result<(), Unspecified> {
        let encoding = padding_alg.encoding();

        let mut md_ctx = digest::digest_ctx::DigestContext::new_uninit();
        let mut pctx = null_mut::<EVP_PKEY_CTX>();
        let digest = digest::match_digest_type(&encoding.digest_algorithm().id);

        if 1 != unsafe {
            // EVP_DigestSignInit does not mutate |pkey| for thread-safety purposes and may be
            // used concurrently with other non-mutating functions on |pkey|.
            // https://github.com/aws/aws-lc/blob/9b4b5a15a97618b5b826d742419ccd54c819fa42/include/openssl/evp.h#L297-L313
            EVP_DigestSignInit(
                md_ctx.as_mut_ptr(),
                &mut pctx,
                *digest,
                null_mut(),
                *self.evp_pkey.as_mut_unsafe(),
            )
        } {
            return Err(Unspecified);
        }

        if let RsaPadding::RSA_PKCS1_PSS_PADDING = encoding.padding() {
            // AWS-LC owns pctx, check for null and then immediately detach so we don't drop it.
            let pctx = DetachableLcPtr::new(pctx)?.detach();
            super::signature::configure_rsa_pkcs1_pss_padding(pctx)?;
        }

        let max_len = super::signature::get_signature_length(&mut md_ctx)?;

        debug_assert!(signature.len() >= max_len);

        let computed_signature = compute_rsa_signature(&mut md_ctx, msg, signature)?;

        debug_assert!(computed_signature.len() >= signature.len());

        Ok(())
    }

    /// Returns the length in bytes of the key pair's public modulus.
    ///
    /// A signature has the same length as the public modulus.
    #[must_use]
    pub fn public_modulus_len(&self) -> usize {
        // This was already validated to be an RSA key so this can't fail
        match self.evp_pkey.get_rsa() {
            Ok(rsa) => {
                // https://github.com/awslabs/aws-lc/blob/main/include/openssl/rsa.h#L99
                unsafe { RSA_size(*rsa.as_const()) as usize }
            }
            Err(_) => unreachable!(),
        }
    }
}

impl Debug for KeyPair {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.write_str(&format!(
            "RsaKeyPair {{ public_key: {:?} }}",
            self.serialized_public_key
        ))
    }
}

impl crate::signature::KeyPair for KeyPair {
    type PublicKey = PublicKey;

    fn public_key(&self) -> &Self::PublicKey {
        &self.serialized_public_key
    }
}

impl AsDer<Pkcs8V1Der<'static>> for KeyPair {
    fn as_der(&self) -> Result<Pkcs8V1Der<'static>, Unspecified> {
        Ok(Pkcs8V1Der::new(encoding::pkcs8::encode_v1_der(
            &self.evp_pkey,
        )?))
    }
}

/// A serialized RSA public key.
#[derive(Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct PublicKey {
    key: Box<[u8]>,
    #[cfg(feature = "ring-io")]
    modulus: Box<[u8]>,
    #[cfg(feature = "ring-io")]
    exponent: Box<[u8]>,
}

impl Drop for PublicKey {
    fn drop(&mut self) {
        self.key.zeroize();
        #[cfg(feature = "ring-io")]
        self.modulus.zeroize();
        #[cfg(feature = "ring-io")]
        self.exponent.zeroize();
    }
}

impl PublicKey {
    pub(super) fn new(evp_pkey: &LcPtr<EVP_PKEY>) -> Result<Self, Unspecified> {
        let key = encoding::rfc8017::encode_public_key_der(evp_pkey)?;
        #[cfg(feature = "ring-io")]
        {
            let pubkey = evp_pkey.get_rsa()?;
            let modulus = ConstPointer::new(unsafe { RSA_get0_n(*pubkey.as_const()) })?;
            let modulus = modulus.to_be_bytes().into_boxed_slice();
            let exponent = ConstPointer::new(unsafe { RSA_get0_e(*pubkey.as_const()) })?;
            let exponent = exponent.to_be_bytes().into_boxed_slice();
            Ok(PublicKey {
                key,
                modulus,
                exponent,
            })
        }

        #[cfg(not(feature = "ring-io"))]
        Ok(PublicKey { key })
    }
}

impl Debug for PublicKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.write_str(&format!(
            "RsaPublicKey(\"{}\")",
            hex::encode(self.key.as_ref())
        ))
    }
}

impl AsRef<[u8]> for PublicKey {
    /// DER encode a RSA public key to (RFC 8017) `RSAPublicKey` structure.
    fn as_ref(&self) -> &[u8] {
        self.key.as_ref()
    }
}

#[cfg(feature = "ring-io")]
impl PublicKey {
    /// The public modulus (n).
    #[must_use]
    pub fn modulus(&self) -> io::Positive<'_> {
        io::Positive::new_non_empty_without_leading_zeros(Input::from(self.modulus.as_ref()))
    }

    /// The public exponent (e).
    #[must_use]
    pub fn exponent(&self) -> io::Positive<'_> {
        io::Positive::new_non_empty_without_leading_zeros(Input::from(self.exponent.as_ref()))
    }
}

/// Low-level API for RSA public keys.
///
/// When the public key is in DER-encoded PKCS#1 ASN.1 format, it is
/// recommended to use `aws_lc_rs::signature::verify()` with
/// `aws_lc_rs::signature::RSA_PKCS1_*`, because `aws_lc_rs::signature::verify()`
/// will handle the parsing in that case. Otherwise, this function can be used
/// to pass in the raw bytes for the public key components as
/// `untrusted::Input` arguments.
#[allow(clippy::module_name_repetitions)]
#[derive(Clone)]
pub struct PublicKeyComponents<B>
where
    B: AsRef<[u8]> + Debug,
{
    /// The public modulus, encoded in big-endian bytes without leading zeros.
    pub n: B,
    /// The public exponent, encoded in big-endian bytes without leading zeros.
    pub e: B,
}

impl<B: AsRef<[u8]> + Debug> Debug for PublicKeyComponents<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaPublicKeyComponents")
            .field("n", &self.n)
            .field("e", &self.e)
            .finish()
    }
}

impl<B: Copy + AsRef<[u8]> + Debug> Copy for PublicKeyComponents<B> {}

impl<B> PublicKeyComponents<B>
where
    B: AsRef<[u8]> + Debug,
{
    #[inline]
    fn build_rsa(&self) -> Result<LcPtr<EVP_PKEY>, ()> {
        let n_bytes = self.n.as_ref();
        if n_bytes.is_empty() || n_bytes[0] == 0u8 {
            return Err(());
        }
        let n_bn = DetachableLcPtr::try_from(n_bytes)?;

        let e_bytes = self.e.as_ref();
        if e_bytes.is_empty() || e_bytes[0] == 0u8 {
            return Err(());
        }
        let e_bn = DetachableLcPtr::try_from(e_bytes)?;

        let rsa = DetachableLcPtr::new(unsafe { RSA_new() })?;
        if 1 != unsafe { RSA_set0_key(*rsa, *n_bn, *e_bn, null_mut()) } {
            return Err(());
        }
        n_bn.detach();
        e_bn.detach();

        let mut pkey = LcPtr::new(unsafe { EVP_PKEY_new() })?;
        if 1 != unsafe { EVP_PKEY_assign_RSA(*pkey.as_mut(), *rsa) } {
            return Err(());
        }
        rsa.detach();

        Ok(pkey)
    }

    /// Verifies that `signature` is a valid signature of `message` using `self`
    /// as the public key. `params` determine what algorithm parameters
    /// (padding, digest algorithm, key length range, etc.) are used in the
    /// verification.
    ///
    /// # Errors
    /// `error::Unspecified` if `message` was not verified.
    pub fn verify(
        &self,
        params: &RsaParameters,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), Unspecified> {
        let rsa = self.build_rsa()?;
        super::signature::verify_rsa_signature(
            params.digest_algorithm(),
            params.padding(),
            &rsa,
            message,
            signature,
            params.bit_size_range(),
        )
    }
}

impl<B> TryInto<PublicEncryptingKey> for PublicKeyComponents<B>
where
    B: AsRef<[u8]> + Debug,
{
    type Error = Unspecified;

    /// Try to build a `PublicEncryptingKey` from the public key components.
    ///
    /// # Errors
    /// `error::Unspecified` if the key failed to verify.
    fn try_into(self) -> Result<PublicEncryptingKey, Self::Error> {
        let rsa = self.build_rsa()?;
        PublicEncryptingKey::new(rsa)
    }
}

pub(super) fn generate_rsa_key(size: c_int, fips: bool) -> Result<LcPtr<EVP_PKEY>, Unspecified> {
    // We explicitly don't use `EVP_PKEY_keygen`, as it will force usage of either the FIPS or non-FIPS
    // keygen function based on the whether the build of AWS-LC had FIPS enbaled. Rather we delegate to the desired
    // generation function.

    const RSA_F4: u64 = 65537;

    let mut rsa = DetachableLcPtr::new(unsafe { RSA_new() })?;

    if 1 != if fips {
        indicator_check!(unsafe { RSA_generate_key_fips(*rsa.as_mut(), size, null_mut()) })
    } else {
        let e: LcPtr<BIGNUM> = RSA_F4.try_into()?;
        unsafe { RSA_generate_key_ex(*rsa.as_mut(), size, *e.as_const(), null_mut()) }
    } {
        return Err(Unspecified);
    }

    let mut evp_pkey = LcPtr::new(unsafe { EVP_PKEY_new() })?;

    if 1 != unsafe { EVP_PKEY_assign_RSA(*evp_pkey.as_mut(), *rsa) } {
        return Err(Unspecified);
    };

    rsa.detach();

    Ok(evp_pkey)
}

#[cfg(feature = "fips")]
#[must_use]
pub(super) fn is_valid_fips_key(key: &LcPtr<EVP_PKEY>) -> bool {
    // This should always be an RSA key and must-never panic.
    let rsa_key = key.get_rsa().expect("RSA EVP_PKEY");

    1 == unsafe { RSA_check_fips(*rsa_key.as_mut_unsafe()) }
}

pub(super) fn key_size_bytes(key: &LcPtr<EVP_PKEY>) -> usize {
    // Safety: RSA modulous byte sizes supported fit an usize
    unsafe { EVP_PKEY_size(*key.as_const()) }
        .try_into()
        .expect("modulous to fit in usize")
}

pub(super) fn key_size_bits(key: &LcPtr<EVP_PKEY>) -> usize {
    // Safety: RSA modulous byte sizes supported fit an usize
    unsafe { EVP_PKEY_bits(*key.as_const()) }
        .try_into()
        .expect("modulous to fit in usize")
}

pub(super) fn is_rsa_key(key: &LcPtr<EVP_PKEY>) -> bool {
    key.get_rsa().is_ok()
}
