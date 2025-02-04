//! Functions for extracting and embedding claims within a WebAssembly module

use crate::{
    errors::{self, ErrorKind},
    jwt::{Actor, Claims, Token, MIN_WASCAP_INTERNAL_REVISION},
    Result,
};
use data_encoding::HEXUPPER;
use nkeys::KeyPair;
use ring::digest::{Context, Digest, SHA256};
use std::{
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};
use wasmparser::BinaryReaderError;
use wasmparser::Payload::*;
const SECS_PER_DAY: u64 = 86400;
const SECTION_JWT: &str = "jwt";
const SECTION_WC_JWT: &str = "wasmcloud_jwt";

/// Extracts a set of claims from the raw bytes of a WebAssembly module. In the case where no
/// JWT is discovered in the module, this function returns `None`.
/// If there is a token in the file with a valid hash, then you will get a `Token` back
/// containing both the raw JWT and the decoded claims.
///
/// # Errors
/// Will return an error if hash computation fails or it can't read the JWT from inside
/// a section's data, etc
pub fn extract_claims(contents: impl AsRef<[u8]>) -> Result<Option<Token<Actor>>> {
    let parser = wasmparser::Parser::new(0);
    for payload in parser.parse_all(contents.as_ref()) {
        match payload? {
            wasmparser::Payload::CustomSection(reader) => {
                if reader.name() == SECTION_JWT || reader.name() == SECTION_WC_JWT {
                    let jwt = String::from_utf8(reader.data().to_vec())?;
                    let claims: Claims<Actor> = Claims::decode(&jwt)?;
                    let hash = compute_hash_without_jwt(contents.as_ref())?;
                    if let Some(ref meta) = claims.metadata {
                        if meta.module_hash != hash
                            && claims.wascap_revision.unwrap_or_default()
                                >= MIN_WASCAP_INTERNAL_REVISION
                        {
                            return Err(errors::new(ErrorKind::InvalidModuleHash));
                        } else {
                            return Ok(Some(Token { jwt, claims }));
                        }
                    } else {
                        return Err(errors::new(ErrorKind::InvalidAlgorithm));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// This function will embed a set of claims inside the bytecode of a WebAssembly module. The claims
/// are converted into a JWT and signed using the provided `KeyPair`.
/// According to the WebAssembly [custom section](https://webassembly.github.io/spec/core/appendix/custom.html)
/// specification, arbitary sets of bytes can be stored in a WebAssembly module without impacting
/// parsers or interpreters. Returns a vector of bytes representing the new WebAssembly module which can
/// be saved to a `.wasm` file
pub fn embed_claims(orig_bytecode: &[u8], claims: &Claims<Actor>, kp: &KeyPair) -> Result<Vec<u8>> {
    let mut bytes = orig_bytecode.to_vec();

    let hash = compute_hash_without_jwt(orig_bytecode)?;
    let mut claims = (*claims).clone();
    let meta = claims.metadata.map(|md| Actor {
        module_hash: hash,
        ..md
    });
    claims.metadata = meta;

    let encoded = claims.encode(kp)?;
    let encvec = encoded.as_bytes().to_vec();
    wasm_gen::write_custom_section(&mut bytes, SECTION_WC_JWT, &encvec);

    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub fn sign_buffer_with_claims(
    name: String,
    buf: impl AsRef<[u8]>,
    mod_kp: KeyPair,
    acct_kp: KeyPair,
    expires_in_days: Option<u64>,
    not_before_days: Option<u64>,
    caps: Vec<String>,
    tags: Vec<String>,
    provider: bool,
    rev: Option<i32>,
    ver: Option<String>,
    call_alias: Option<String>,
) -> Result<Vec<u8>> {
    let claims = Claims::<Actor>::with_dates(
        name,
        acct_kp.public_key(),
        mod_kp.public_key(),
        Some(caps),
        Some(tags),
        days_from_now_to_jwt_time(not_before_days),
        days_from_now_to_jwt_time(expires_in_days),
        provider,
        rev,
        ver,
        call_alias,
    );
    embed_claims(buf.as_ref(), &claims, &acct_kp)
}

fn since_the_epoch() -> std::time::Duration {
    let start = SystemTime::now();
    start
        .duration_since(UNIX_EPOCH)
        .expect("A timey wimey problem has occurred!")
}

pub fn days_from_now_to_jwt_time(stamp: Option<u64>) -> Option<u64> {
    stamp.map(|e| since_the_epoch().as_secs() + e * SECS_PER_DAY)
}

fn sha256_digest<R: Read>(mut reader: R) -> Result<Digest> {
    let mut context = Context::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }

    Ok(context.finish())
}

// NOTE: we don't need to compute a hash of the entire file, we just need
// to compute the hash if the things that indicate tampering, like code and
// custom sections
fn compute_hash_without_jwt(modbytes: &[u8]) -> Result<String> {
    let mut binary: Vec<u8> = Vec::new();
    let parser = wasmparser::Parser::new(0);

    for payload in parser.parse_all(modbytes) {
        match payload? {
            CodeSectionEntry(fb) => {
                let mut rdr = fb.get_binary_reader();
                let remaining = rdr.bytes_remaining();
                binary.extend_from_slice(
                    rdr.read_bytes(remaining)
                        .map_err(|e| BinaryReaderError::from(e))?,
                );
            }
            DataSection(mut reader) => {
                binary
                    .extend_from_slice(reader.read().map_err(|e| BinaryReaderError::from(e))?.data);
            }
            CustomSection(reader) => {
                if reader.name() != SECTION_JWT && reader.name() != SECTION_WC_JWT {
                    binary.extend_from_slice(reader.data());
                }
            }
            _ => {}
        }
    }

    let digest = sha256_digest(binary.as_slice())?;
    Ok(HEXUPPER.encode(digest.as_ref()))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        caps::{KEY_VALUE, LOGGING, MESSAGING},
        jwt::{Actor, Claims, WASCAP_INTERNAL_REVISION},
    };
    use base64::decode;

    const WASM_BASE64: &str =
        "AGFzbQEAAAAADAZkeWxpbmuAgMACAAGKgICAAAJgAn9/AX9gAAACwYCAgAAEA2VudgptZW1vcnlCYXNl\
         A38AA2VudgZtZW1vcnkCAIACA2VudgV0YWJsZQFwAAADZW52CXRhYmxlQmFzZQN/AAOEgICAAAMAAQEGi\
         4CAgAACfwFBAAt/AUEACwejgICAAAIKX3RyYW5zZm9ybQAAEl9fcG9zdF9pbnN0YW50aWF0ZQACCYGAgI\
         AAAArpgICAAAPBgICAAAECfwJ/IABBAEoEQEEAIQIFIAAPCwNAIAEgAmoiAywAAEHpAEYEQCADQfkAOgA\
         ACyACQQFqIgIgAEcNAAsgAAsLg4CAgAAAAQuVgICAAAACQCMAJAIjAkGAgMACaiQDEAELCw==";

    #[test]
    fn claims_roundtrip() {
        // Serialize and de-serialize this because the module loader adds bytes to
        // the above base64 encoded module.
        let dec_module = decode(WASM_BASE64).unwrap();

        let kp = KeyPair::new_account();
        let claims = Claims {
            metadata: Some(Actor::new(
                "testing".to_string(),
                Some(vec![MESSAGING.to_string(), KEY_VALUE.to_string()]),
                Some(vec![]),
                false,
                Some(1),
                Some("".to_string()),
                None,
            )),
            expires: None,
            id: nuid::next(),
            issued_at: 0,
            issuer: kp.public_key(),
            subject: "test.wasm".to_string(),
            not_before: None,
            wascap_revision: Some(WASCAP_INTERNAL_REVISION),
        };
        let modified_bytecode = embed_claims(&dec_module, &claims, &kp).unwrap();
        println!(
            "Added {} bytes in custom section.",
            modified_bytecode.len() - dec_module.len()
        );
        if let Some(token) = extract_claims(&modified_bytecode).unwrap() {
            assert_eq!(claims.issuer, token.claims.issuer);
            assert_eq!(
                claims.metadata.as_ref().unwrap().caps,
                token.claims.metadata.as_ref().unwrap().caps
            );
        } else {
            unreachable!()
        }
    }

    #[test]
    fn claims_logging_roundtrip() {
        // Serialize and de-serialize this because the module loader adds bytes to
        // the above base64 encoded module.
        let dec_module = decode(WASM_BASE64).unwrap();

        let kp = KeyPair::new_account();
        let claims = Claims {
            metadata: Some(Actor::new(
                "testing".to_string(),
                Some(vec![MESSAGING.to_string(), LOGGING.to_string()]),
                Some(vec![]),
                false,
                Some(1),
                Some("".to_string()),
                None,
            )),
            expires: None,
            id: nuid::next(),
            issued_at: 0,
            issuer: kp.public_key(),
            subject: "test.wasm".to_string(),
            not_before: None,
            wascap_revision: Some(WASCAP_INTERNAL_REVISION),
        };
        let modified_bytecode = embed_claims(&dec_module, &claims, &kp).unwrap();
        println!(
            "Added {} bytes in custom section.",
            modified_bytecode.len() - dec_module.len()
        );
        if let Some(token) = extract_claims(&modified_bytecode).unwrap() {
            assert_eq!(claims.issuer, token.claims.issuer);
            assert_eq!(claims.subject, token.claims.subject);
        } else {
            unreachable!()
        }
    }
}
