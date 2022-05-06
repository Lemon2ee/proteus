use crate::net::proto::upgen::crypto::{self, CryptoProtocol};

use std::cmp;
use std::io::Cursor;
use std::rc::Rc;

use bytes::{buf, Buf, Bytes};

use rand::rngs::OsRng;
use rand::RngCore;

use chacha20poly1305::aead::{Aead, NewAead};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use salsa20::cipher::{KeyIvInit, StreamCipher};
use salsa20::Salsa20;

use x25519_dalek::{x25519, PublicKey, SharedSecret, StaticSecret};

enum CipherKind {
    Sender,
    Receiver,
}

struct Cipher {
    encryption_nonce_gen: Salsa20,
    decryption_nonce_gen: Salsa20,
    encryption_cipher: ChaCha20Poly1305,
    decryption_cipher: ChaCha20Poly1305,
}

impl Cipher {
    pub fn new(secret_key: [u8; 32], cipher_kind: CipherKind) -> Cipher {
        const NONCE_A: [u8; 8] = [0xAA; 8];
        const NONCE_B: [u8; 8] = [0xBB; 8];

        let encryption_nonce = match cipher_kind {
            CipherKind::Sender => NONCE_A,
            CipherKind::Receiver => NONCE_B,
        };

        let decryption_nonce = match cipher_kind {
            CipherKind::Sender => NONCE_B,
            CipherKind::Receiver => NONCE_A,
        };

        Cipher {
            encryption_nonce_gen: Salsa20::new(&secret_key.into(), &encryption_nonce.into()),
            decryption_nonce_gen: Salsa20::new(&secret_key.into(), &decryption_nonce.into()),
            encryption_cipher: ChaCha20Poly1305::new(&secret_key.into()),
            decryption_cipher: ChaCha20Poly1305::new(&secret_key.into()),
        }
    }

    fn get_nonce(nonce_gen: &mut Salsa20) -> [u8; 12] {
        let mut buf: [u8; 12] = [0x00; 12];
        nonce_gen.apply_keystream(&mut buf);
        buf
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let nonce = Cipher::get_nonce(&mut self.encryption_nonce_gen);

        self.encryption_cipher
            .encrypt(&nonce.into(), plaintext)
            .expect("encryption failure")
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Vec<u8> {
        let nonce = Cipher::get_nonce(&mut self.decryption_nonce_gen);

        self.decryption_cipher
            .decrypt(&nonce.into(), ciphertext)
            .expect("decryption failure")
    }
}

#[derive(Clone)]
pub struct CryptoModule {
    my_secret_key: StaticSecret,
    my_public_key: Option<PublicKey>,
    our_shared_secret_key: Option<[u8; 32]>,
    cipher: Option<Rc<Cipher>>,
}

impl CryptoModule {
    pub fn new() -> CryptoModule {
        let mut key: [u8; 32] = [0; 32];
        OsRng.fill_bytes(&mut key);

        CryptoModule {
            my_secret_key: StaticSecret::from(key),
            my_public_key: None,
            our_shared_secret_key: None,
            cipher: None,
        }
    }

    fn produce_my_half_handshake(&mut self) -> [u8; 32] {
        self.my_public_key = Some(PublicKey::from(&self.my_secret_key));
        self.my_public_key.unwrap().to_bytes()
    }

    /*
     * Produces the shared secret and instantiates the ciphers.
     */
    fn receive_their_half_handshake(&mut self, their_public_key: [u8; 32]) {
        self.our_shared_secret_key = Some(x25519(self.my_secret_key.to_bytes(), their_public_key));

        let mut my_role = CipherKind::Sender;

        if their_public_key < self.my_public_key.unwrap().to_bytes() {
            my_role = CipherKind::Receiver;
        } else {
        }

        self.cipher = Some(Rc::new(Cipher::new(
            self.our_shared_secret_key.unwrap(),
            my_role,
        )));
    }

    fn encrypt_impl(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut cipher = self.cipher.as_mut().unwrap();
        Rc::get_mut(&mut cipher).unwrap().encrypt(plaintext)
    }

    fn decrypt_impl(&mut self, ciphertext: &[u8]) -> Vec<u8> {
        let mut cipher = self.cipher.as_mut().unwrap();
        Rc::get_mut(&mut cipher).unwrap().decrypt(ciphertext)
    }

    pub fn encrypt(
        &mut self,
        plaintext: &mut Cursor<Bytes>,
        ciphertext_len: usize,
    ) -> Result<Bytes, crypto::Error> {

        if ciphertext_len < CryptoModule::suggest_ciphertext_nbytes(0) {
            return Err(crypto::Error::CryptFailure);
        }

        let plaintext_nbytes_available = plaintext.remaining();

        // If you want to write more than I have available, we're gonna crash for now.
        let plaintext_nbytes_required =
            CryptoModule::determine_plaintext_size(ciphertext_len).unwrap();

        if plaintext_nbytes_available < plaintext_nbytes_required {
            // FIXME(rwails) Eventually, this shouldn't produce any error.
            unimplemented!("not enough plaintext available.");
        }

        // Otherwise, we know we have enough data to fulfill the request.
        let plaintext_tmp = plaintext.copy_to_bytes(plaintext_nbytes_required);

        Ok(Bytes::from(self.encrypt_impl(&plaintext_tmp)))
    }

    pub fn decrypt(&mut self, ciphertext: &Bytes) -> Result<Bytes, crypto::Error> {
        Ok(Bytes::from(self.decrypt_impl(&ciphertext)))
    }

    pub fn suggest_ciphertext_nbytes(plaintext_len: usize) -> usize {
        plaintext_len + 16 // Poly1305 MAC tag is 16 bytes
    }

    fn determine_plaintext_size(ciphertext_len: usize) -> Option<usize> {
        // FIXME(rwails)
        // This doesn't always work right currently.
        usize::try_from(isize::try_from(ciphertext_len).unwrap() - 16isize).ok()
    }
}

/*
impl CryptoProtocol for CryptoModule {
    fn encrypt(&mut self, plaintext: &Bytes) -> Result<Bytes, crypto::Error> {
        todo!()
    }

    fn decrypt(&mut self, ciphertext: &Bytes) -> Result<Bytes, crypto::Error> {
        todo!()
    }

    fn len(&self, material: crypto::CryptoMaterialKind) -> usize {
        todo!()
    }
}
*/

#[cfg(test)]
mod tests {

    use super::*;

    fn get_connected_pair() -> (CryptoModule, CryptoModule) {
        let mut alice = CryptoModule::new();
        let mut bob = CryptoModule::new();

        let alice_key = alice.produce_my_half_handshake();
        let bob_key = bob.produce_my_half_handshake();

        alice.receive_their_half_handshake(bob_key);
        bob.receive_their_half_handshake(alice_key);

        (alice, bob)
    }

    // #[test]
    fn test_key_exchange() {
        let (alice, bob) = get_connected_pair();

        assert_eq!(alice.our_shared_secret_key, bob.our_shared_secret_key);
        assert!(!alice.our_shared_secret_key.is_none());
    }

    #[test]
    fn test_encryption_decryption() {
        let (mut alice, mut bob) = get_connected_pair();

        let plaintext = b"hello world";

        let ciphertext = alice.encrypt_impl(plaintext);

        let plaintext_prime = bob.decrypt_impl(&ciphertext);

        assert_eq!(plaintext, &plaintext_prime[..]);
    }

    #[test]
    fn test_crypto_conversation() {
        let (mut alice, mut bob) = get_connected_pair();

        let alice_plaintext = b"hello world";
        let bob_plaintext = b"lorem ipsum";

        let alice_ciphertext = alice.encrypt_impl(alice_plaintext);
        let bob_ciphertext = bob.encrypt_impl(bob_plaintext);

        let alice_plaintext_prime = bob.decrypt_impl(&alice_ciphertext);
        assert_eq!(alice_plaintext, &alice_plaintext_prime[..]);

        let bob_plaintext_prime = alice.decrypt_impl(&bob_ciphertext);
        assert_eq!(bob_plaintext, &bob_plaintext_prime[..]);
    }

    #[test]
    fn test_two_message_encryption() {
        let (mut alice, mut bob) = get_connected_pair();

        let plaintext = b"hello world";

        let ciphertext1 = alice.encrypt_impl(plaintext);
        let ciphertext2 = alice.encrypt_impl(plaintext);
        assert_ne!(ciphertext1, ciphertext2);

        let plaintext_prime1 = bob.decrypt_impl(&ciphertext1);
        let plaintext_prime2 = bob.decrypt_impl(&ciphertext2);

        assert_eq!(plaintext, &plaintext_prime1[..]);
        assert_eq!(plaintext, &plaintext_prime2[..]);
    }

    #[test]
    fn test_suggested_size() {
        let (mut alice, _) = get_connected_pair();

        let plaintext = b"hello world";
        let ciphertext = alice.encrypt_impl(plaintext);

        assert_eq!(
            ciphertext.len(),
            CryptoModule::suggest_ciphertext_nbytes(plaintext.len())
        );
    }

    #[test]
    fn test_public_api() {
        let (mut alice, mut bob) = get_connected_pair();

        let plaintext = Bytes::from("hello world");
        let mut cursor = Cursor::new(plaintext);

        let ciphertext_nbytes = CryptoModule::suggest_ciphertext_nbytes(cursor.remaining());
        let ciphertext = alice.encrypt(&mut cursor, ciphertext_nbytes).unwrap();

        assert_eq!(ciphertext.len(), ciphertext_nbytes);
        assert_eq!(cursor.remaining(), 0);

        let plaintext_prime = bob.decrypt(&ciphertext).unwrap();
        assert_eq!(cursor.into_inner(), plaintext_prime);
    }

    #[test]
    fn test_public_api_partial_write() {
        let (mut alice, mut bob) = get_connected_pair();

        let plaintext = Bytes::from("hello world");
        let original_plaintext_nbytes = plaintext.len();

        let mut cursor = Cursor::new(plaintext);

        // Let's setup a very small frame limit.
        const FRAME_LIMIT_NBYTES: usize = 17;

        let ciphertext_nbytes =
            std::cmp::min(
                CryptoModule::suggest_ciphertext_nbytes(cursor.remaining()),
                FRAME_LIMIT_NBYTES
            );

        let ciphertext = alice.encrypt(&mut cursor, ciphertext_nbytes).unwrap();

        assert_eq!(ciphertext.len(), ciphertext_nbytes);

        // We should have only been able to write one byte of plaintext.
        assert_eq!(cursor.remaining(), original_plaintext_nbytes - 1);

        let plaintext_prime = bob.decrypt(&ciphertext).unwrap();
        assert_eq!(Bytes::from("h"), plaintext_prime);
    }
}
