//! Logging in to a Soul Injector over the network.
//!
//! The device runs the session handshake of ESP-IDF's protocomm Security 2:
//! SRP6a with the 3072-bit group of RFC 5054 and SHA-512. It keeps only a salt
//! and a verifier, made here from the password during setup over USB. The
//! password never crosses the network, and each side proves that it knows it.
//!
//! Only the handshake is used. Protocomm's encryption keeps one cipher state
//! for strict request-then-response exchanges and cannot carry SIDP's
//! unsolicited events, so traffic after the login is not encrypted.
//!
//! The arithmetic follows the device's `esp_srp.c` byte for byte. Espressif's
//! Python client (`srp6a.py`) gives the same results except for the 1 in 256
//! passwords whose hash starts with a zero byte, which it shortens.
use num_bigint::BigUint;
use sha2::{Digest, Sha512};
use std::io;

/// User name sent in the handshake; verifiers are made for it.
pub const USERNAME: &str = "sidp";
/// Bytes of the group prime, and of the public key the device accepts.
const KEY_LEN: usize = 384;
const SALT_LEN: usize = 16;
const SECRET_LEN: usize = 32;
const G: u8 = 5;
/// RFC 5054's 3072-bit prime, as in `esp_srp.c`.
const N_HEX: [&str; 12] = [
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
];

/// What the device stores to check a password: the password cannot be
/// recovered from it, though a short one can be guessed offline.
pub struct PasswordVerifier {
    /// Random salt.
    pub salt: Vec<u8>,
    /// g^x mod N.
    pub verifier: Vec<u8>,
}

/// Makes a verifier for `password` with a fresh random salt.
pub fn make_verifier(password: &str) -> io::Result<PasswordVerifier> {
    let mut salt = vec![0; SALT_LEN];
    getrandom::fill(&mut salt).map_err(io::Error::other)?;
    let verifier = verifier_for(password, &salt);
    Ok(PasswordVerifier { salt, verifier })
}

fn verifier_for(password: &str, salt: &[u8]) -> Vec<u8> {
    BigUint::from(G)
        .modpow(&x(salt, password), &n())
        .to_bytes_be()
}

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.concat().as_bytes(), 16).expect("valid prime")
}

/// Left-pads `bytes` with zeros to the prime's length.
fn pad(bytes: &[u8]) -> Vec<u8> {
    let mut padded = vec![0; KEY_LEN.saturating_sub(bytes.len())];
    padded.extend_from_slice(bytes);
    padded
}

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut sha = Sha512::new();
    for part in parts {
        sha.update(part);
    }
    sha.finalize().to_vec()
}

/// x = H(salt | H(user ":" password)).
fn x(salt: &[u8], password: &str) -> BigUint {
    let inner = hash(&[USERNAME.as_bytes(), b":", password.as_bytes()]);
    BigUint::from_bytes_be(&hash(&[salt, &inner]))
}

/// The client side of one login.
pub struct Client {
    password: String,
    a: BigUint,
    public_key: Vec<u8>,
}

impl Client {
    /// Starts a login with a fresh random secret.
    pub fn new(password: &str) -> io::Result<Self> {
        let mut secret = [0; SECRET_LEN];
        getrandom::fill(&mut secret).map_err(io::Error::other)?;
        Ok(Self::with_secret(password, BigUint::from_bytes_be(&secret)))
    }

    fn with_secret(password: &str, a: BigUint) -> Self {
        let public_key = pad(&BigUint::from(G).modpow(&a, &n()).to_bytes_be());
        Self {
            password: password.to_owned(),
            a,
            public_key,
        }
    }

    /// A = g^a mod N, always 384 bytes: the device refuses any other length.
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// The client's proof M, and the proof the device must answer with, from
    /// the device's salt and public key B. `None` when B is invalid.
    pub fn proofs(&self, salt: &[u8], device_key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let n = n();
        let g = BigUint::from(G);
        let b = BigUint::from_bytes_be(device_key);
        if (&b % &n) == BigUint::ZERO {
            return None;
        }
        let k = BigUint::from_bytes_be(&hash(&[&pad(&n.to_bytes_be()), &pad(&[G])]));
        let u = BigUint::from_bytes_be(&hash(&[&self.public_key, &pad(device_key)]));
        if u == BigUint::ZERO {
            return None;
        }
        let x = x(salt, &self.password);
        let kv = (k * g.modpow(&x, &n)) % &n;
        // S = (B - k v)^(a + u x) mod N
        let base = (&b % &n + &n - kv) % &n;
        let s = base.modpow(&(&self.a + u * x), &n);
        let key = hash(&[&s.to_bytes_be()]);

        let hash_n = hash(&[&n.to_bytes_be()]);
        let hash_g = hash(&[&pad(&[G])]);
        let n_xor_g: Vec<u8> = hash_n.iter().zip(&hash_g).map(|(n, g)| n ^ g).collect();
        let m = hash(&[
            &n_xor_g,
            &hash(&[USERNAME.as_bytes()]),
            salt,
            &self.public_key,
            device_key,
            &key,
        ]);
        let device_proof = hash(&[&self.public_key, &m, &key]);
        Some((m, device_proof))
    }
}

/// Protocomm's handshake messages (`session.proto` and `sec2.proto` in
/// ESP-IDF's protocomm component). Oneof members are plain optional fields,
/// which encode the same way.
pub mod messages {
    /// `SessionData`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SessionData {
        /// Security scheme version: 2.
        #[prost(int32, tag = "2")]
        pub sec_ver: i32,
        /// Security 2 payload.
        #[prost(message, optional, tag = "12")]
        pub sec2: Option<Sec2Payload>,
    }

    /// `Sec2Payload`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Sec2Payload {
        /// Message type: 0 command 0, 1 response 0, 2 command 1, 3 response 1.
        #[prost(int32, tag = "1")]
        pub msg: i32,
        /// Command 0.
        #[prost(message, optional, tag = "20")]
        pub sc0: Option<Command0>,
        /// Response 0.
        #[prost(message, optional, tag = "21")]
        pub sr0: Option<Response0>,
        /// Command 1.
        #[prost(message, optional, tag = "22")]
        pub sc1: Option<Command1>,
        /// Response 1.
        #[prost(message, optional, tag = "23")]
        pub sr1: Option<Response1>,
    }

    /// `S2SessionCmd0`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Command0 {
        /// User name.
        #[prost(bytes = "vec", tag = "1")]
        pub client_username: Vec<u8>,
        /// A, 384 bytes.
        #[prost(bytes = "vec", tag = "2")]
        pub client_pubkey: Vec<u8>,
    }

    /// `S2SessionResp0`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Response0 {
        /// Protocomm status; 0 is success.
        #[prost(int32, tag = "1")]
        pub status: i32,
        /// B.
        #[prost(bytes = "vec", tag = "2")]
        pub device_pubkey: Vec<u8>,
        /// Salt.
        #[prost(bytes = "vec", tag = "3")]
        pub device_salt: Vec<u8>,
    }

    /// `S2SessionCmd1`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Command1 {
        /// M.
        #[prost(bytes = "vec", tag = "1")]
        pub client_proof: Vec<u8>,
    }

    /// `S2SessionResp1`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Response1 {
        /// Protocomm status; 0 is success.
        #[prost(int32, tag = "1")]
        pub status: i32,
        /// H(A | M | K).
        #[prost(bytes = "vec", tag = "2")]
        pub device_proof: Vec<u8>,
        /// Nonce for protocomm's encryption; unused.
        #[prost(bytes = "vec", tag = "3")]
        pub device_nonce: Vec<u8>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(name: &str) -> Vec<u8> {
        let text = include_str!("../tests/fixtures/srp6a_vectors.txt");
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .unwrap();
        (0..line.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
            .collect()
    }

    const PASSWORD: &str = "correct horse battery staple";

    /// Vectors from Espressif's reference client; see tests/fixtures.
    #[test]
    fn matches_espressifs_reference_client() {
        let salt = vector("salt");
        assert_eq!(verifier_for(PASSWORD, &salt), vector("verifier"));
        let client = Client::with_secret(PASSWORD, BigUint::from_bytes_be(&vector("a")));
        assert_eq!(client.public_key(), vector("A"));
        let (m, device_proof) = client.proofs(&salt, &vector("B")).unwrap();
        assert_eq!(m, vector("M"));
        assert_eq!(device_proof, vector("H_AMK"));
    }

    #[test]
    fn a_wrong_password_gives_another_proof() {
        let salt = vector("salt");
        let client = Client::with_secret("wrong", BigUint::from_bytes_be(&vector("a")));
        let (m, _) = client.proofs(&salt, &vector("B")).unwrap();
        assert_ne!(m, vector("M"));
    }

    #[test]
    fn refuses_a_zero_device_key() {
        let client = Client::new(PASSWORD).unwrap();
        assert!(client.proofs(&[1; 16], &n().to_bytes_be()).is_none());
        assert_eq!(client.public_key().len(), KEY_LEN);
    }

    #[test]
    fn verifiers_get_fresh_salt() {
        let one = make_verifier(PASSWORD).unwrap();
        let two = make_verifier(PASSWORD).unwrap();
        assert_eq!(one.salt.len(), SALT_LEN);
        assert_ne!(one.salt, two.salt);
        assert_ne!(one.verifier, two.verifier);
    }

    /// Logs in to ESP-IDF's own Security 2 handler, built for this computer
    /// by tests/e2e/srp_device/build.sh and named by SIDP_SRP_DEVICE. Many
    /// rounds, so that salts and hashes with leading zero bytes come up.
    #[test]
    #[ignore = "needs SIDP_SRP_DEVICE, ESP-IDF's Security 2 handler built for this computer"]
    fn logs_in_to_espressifs_handler() {
        use super::messages::{Command0, Command1, Sec2Payload, SessionData};
        use prost::Message;
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let device_path = std::env::var("SIDP_SRP_DEVICE").unwrap();
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let unhex = |text: &str| -> Vec<u8> {
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
                .collect()
        };
        for round in 0..300 {
            let password = format!("password {round}");
            let stored = make_verifier(&password).unwrap();
            let mut device = Command::new(&device_path)
                .args([hex(&stored.salt), hex(&stored.verifier)])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = device.stdin.take().unwrap();
            let mut output = BufReader::new(device.stdout.take().unwrap());
            let mut exchange = |payload: Sec2Payload| -> Option<Sec2Payload> {
                let request = SessionData {
                    sec_ver: 2,
                    sec2: Some(payload),
                };
                writeln!(input, "{}", hex(&request.encode_to_vec())).unwrap();
                let mut line = String::new();
                output.read_line(&mut line).unwrap();
                let line = line.trim();
                (line != "ERR").then(|| SessionData::decode(&*unhex(line)).unwrap().sec2.unwrap())
            };

            // Every tenth round uses a wrong password, which the device must refuse.
            let wrong = round % 10 == 9;
            let client = Client::new(if wrong { "wrong" } else { &password }).unwrap();
            let challenge = exchange(Sec2Payload {
                msg: 0,
                sc0: Some(Command0 {
                    client_username: USERNAME.as_bytes().to_vec(),
                    client_pubkey: client.public_key().to_vec(),
                }),
                ..Default::default()
            })
            .unwrap()
            .sr0
            .unwrap();
            assert_eq!(challenge.device_salt, stored.salt);
            let (proof, device_proof) = client
                .proofs(&challenge.device_salt, &challenge.device_pubkey)
                .unwrap();
            let answer = exchange(Sec2Payload {
                msg: 2,
                sc1: Some(Command1 {
                    client_proof: proof,
                }),
                ..Default::default()
            });
            if wrong {
                assert!(answer.is_none(), "round {round}: wrong password accepted");
            } else {
                let answer = answer
                    .expect("device refused the right password")
                    .sr1
                    .unwrap();
                assert_eq!(answer.device_proof, device_proof, "round {round}");
            }
            drop(input);
            device.wait().unwrap();
        }
    }
}
