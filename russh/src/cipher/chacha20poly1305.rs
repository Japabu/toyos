// Copyright 2016 Pierre-Étienne Meunier
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

// http://cvsweb.openbsd.org/cgi-bin/cvsweb/src/usr.bin/ssh/PROTOCOL.chacha20poly1305?annotate=HEAD

// --- ring / aws-lc-rs backend ---

#[cfg(any(feature = "ring", feature = "aws-lc-rs"))]
mod ring_or_aws {
    #[cfg(feature = "aws-lc-rs")]
    use aws_lc_rs::aead::chacha20_poly1305_openssh;
    #[cfg(all(not(feature = "aws-lc-rs"), feature = "ring"))]
    use ring::aead::chacha20_poly1305_openssh;

    use super::super::super::Error;
    use crate::mac::MacAlgorithm;

    pub struct SshChacha20Poly1305Cipher {}

    impl super::super::Cipher for SshChacha20Poly1305Cipher {
        fn key_len(&self) -> usize {
            chacha20_poly1305_openssh::KEY_LEN
        }

        fn make_opening_key(
            &self,
            k: &[u8],
            _: &[u8],
            _: &[u8],
            _: &dyn MacAlgorithm,
        ) -> Box<dyn super::super::OpeningKey + Send> {
            Box::new(OpeningKey(chacha20_poly1305_openssh::OpeningKey::new(
                #[allow(clippy::unwrap_used)]
                k.try_into().unwrap(),
            )))
        }

        fn make_sealing_key(
            &self,
            k: &[u8],
            _: &[u8],
            _: &[u8],
            _: &dyn MacAlgorithm,
        ) -> Box<dyn super::super::SealingKey + Send> {
            Box::new(SealingKey(chacha20_poly1305_openssh::SealingKey::new(
                #[allow(clippy::unwrap_used)]
                k.try_into().unwrap(),
            )))
        }
    }

    pub struct OpeningKey(chacha20_poly1305_openssh::OpeningKey);

    pub struct SealingKey(chacha20_poly1305_openssh::SealingKey);

    impl super::super::OpeningKey for OpeningKey {
        fn decrypt_packet_length(
            &self,
            sequence_number: u32,
            encrypted_packet_length: &[u8],
        ) -> [u8; 4] {
            self.0.decrypt_packet_length(
                sequence_number,
                #[allow(clippy::unwrap_used)]
                encrypted_packet_length.try_into().unwrap(),
            )
        }

        fn tag_len(&self) -> usize {
            chacha20_poly1305_openssh::TAG_LEN
        }

        fn open<'a>(
            &mut self,
            sequence_number: u32,
            ciphertext_and_tag: &'a mut [u8],
        ) -> Result<&'a [u8], Error> {
            let ciphertext_len = ciphertext_and_tag.len() - self.tag_len();
            let (ciphertext_in_plaintext_out, tag) =
                ciphertext_and_tag.split_at_mut(ciphertext_len);

            self.0
                .open_in_place(
                    sequence_number,
                    ciphertext_in_plaintext_out,
                    #[allow(clippy::unwrap_used)]
                    &tag.try_into().unwrap(),
                )
                .map_err(|_| Error::DecryptionError)
        }
    }

    impl super::super::SealingKey for SealingKey {
        fn padding_length(&self, payload: &[u8]) -> usize {
            let block_size = 8;
            let extra_len = super::super::PACKET_LENGTH_LEN + super::super::PADDING_LENGTH_LEN;
            let padding_len = if payload.len() + extra_len <= super::super::MINIMUM_PACKET_LEN {
                super::super::MINIMUM_PACKET_LEN - payload.len() - super::super::PADDING_LENGTH_LEN
            } else {
                block_size - ((super::super::PADDING_LENGTH_LEN + payload.len()) % block_size)
            };
            if padding_len < super::super::PACKET_LENGTH_LEN {
                padding_len + block_size
            } else {
                padding_len
            }
        }

        // As explained in "SSH via CTR mode with stateful decryption" in
        // https://openvpn.net/papers/ssh-security.pdf, the padding doesn't need to
        // be random because we're doing stateful counter-mode encryption. Use
        // fixed padding to avoid PRNG overhead.
        fn fill_padding(&self, padding_out: &mut [u8]) {
            for padding_byte in padding_out {
                *padding_byte = 0;
            }
        }

        fn tag_len(&self) -> usize {
            chacha20_poly1305_openssh::TAG_LEN
        }

        fn seal(
            &mut self,
            sequence_number: u32,
            plaintext_in_ciphertext_out: &mut [u8],
            tag: &mut [u8],
        ) {
            self.0.seal_in_place(
                sequence_number,
                plaintext_in_ciphertext_out,
                #[allow(clippy::unwrap_used)]
                tag.try_into().unwrap(),
            );
        }
    }
}

#[cfg(any(feature = "ring", feature = "aws-lc-rs"))]
pub use ring_or_aws::SshChacha20Poly1305Cipher;

// --- Pure-Rust rustcrypto backend ---
//
// Implements chacha20-poly1305@openssh.com per:
// https://cvsweb.openbsd.org/src/usr.bin/ssh/PROTOCOL.chacha20poly1305?annotate=HEAD
//
// This uses two ChaCha20 keys (K2 for payload, K1 for packet length) with the
// original djb 64-bit nonce variant, and unpadded Poly1305 MAC computation.

#[cfg(all(feature = "rustcrypto", not(any(feature = "ring", feature = "aws-lc-rs"))))]
mod pure_rust {
    use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
    use chacha20::ChaCha20Legacy;
    use poly1305::Poly1305;
    use poly1305::universal_hash::KeyInit;
    use subtle::ConstantTimeEq;

    use super::super::super::Error;
    use crate::mac::MacAlgorithm;

    /// Total key length: 64 bytes (two 32-byte ChaCha20 keys)
    const KEY_LEN: usize = 64;
    const TAG_LEN: usize = 16;

    pub struct SshChacha20Poly1305Cipher {}

    impl super::super::Cipher for SshChacha20Poly1305Cipher {
        fn key_len(&self) -> usize {
            KEY_LEN
        }

        fn make_opening_key(
            &self,
            k: &[u8],
            _: &[u8],
            _: &[u8],
            _: &dyn MacAlgorithm,
        ) -> Box<dyn super::super::OpeningKey + Send> {
            #[allow(clippy::unwrap_used)]
            let key: [u8; KEY_LEN] = k.try_into().unwrap();
            Box::new(OpeningKey(key))
        }

        fn make_sealing_key(
            &self,
            k: &[u8],
            _: &[u8],
            _: &[u8],
            _: &dyn MacAlgorithm,
        ) -> Box<dyn super::super::SealingKey + Send> {
            #[allow(clippy::unwrap_used)]
            let key: [u8; KEY_LEN] = k.try_into().unwrap();
            Box::new(SealingKey(key))
        }
    }

    /// Build an 8-byte nonce from a sequence number (big-endian, zero-padded).
    fn make_nonce(sequence_number: u32) -> [u8; 8] {
        let mut nonce = [0u8; 8];
        #[allow(clippy::indexing_slicing)]
        nonce[4..8].copy_from_slice(&sequence_number.to_be_bytes());
        nonce
    }

    /// Create a ChaCha20Legacy cipher instance from a 32-byte key and 8-byte nonce.
    fn make_chacha(key: &[u8; 32], nonce: &[u8; 8]) -> ChaCha20Legacy {
        ChaCha20Legacy::new(key.into(), nonce.into())
    }

    /// Derive the one-time Poly1305 key from K2 by encrypting 32 zero bytes
    /// at counter position 0.
    fn derive_poly1305_key(k2: &[u8; 32], nonce: &[u8; 8]) -> poly1305::Key {
        let mut poly_key = [0u8; 32];
        let mut cipher = make_chacha(k2, nonce);
        cipher.apply_keystream(&mut poly_key);
        poly_key.into()
    }

    /// Split the 64-byte key into K2 (first 32 bytes, payload) and K1 (second 32 bytes, length).
    fn split_key(key: &[u8; KEY_LEN]) -> (&[u8; 32], &[u8; 32]) {
        #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
        let k2: &[u8; 32] = key[..32].try_into().unwrap();
        #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
        let k1: &[u8; 32] = key[32..].try_into().unwrap();
        (k2, k1)
    }

    pub struct OpeningKey([u8; KEY_LEN]);
    pub struct SealingKey([u8; KEY_LEN]);

    impl super::super::OpeningKey for OpeningKey {
        fn decrypt_packet_length(
            &self,
            sequence_number: u32,
            encrypted_packet_length: &[u8],
        ) -> [u8; 4] {
            let (_, k1) = split_key(&self.0);
            let nonce = make_nonce(sequence_number);
            let mut len_buf = [0u8; 4];
            #[allow(clippy::indexing_slicing)]
            len_buf.copy_from_slice(&encrypted_packet_length[..4]);
            let mut cipher = make_chacha(k1, &nonce);
            cipher.apply_keystream(&mut len_buf);
            len_buf
        }

        fn tag_len(&self) -> usize {
            TAG_LEN
        }

        fn open<'a>(
            &mut self,
            sequence_number: u32,
            ciphertext_and_tag: &'a mut [u8],
        ) -> Result<&'a [u8], Error> {
            let (k2, _) = split_key(&self.0);
            let nonce = make_nonce(sequence_number);

            let ciphertext_len = ciphertext_and_tag.len() - TAG_LEN;
            let (ciphertext, tag) = ciphertext_and_tag.split_at_mut(ciphertext_len);

            // Verify Poly1305 tag over ciphertext (including encrypted length)
            let poly_key = derive_poly1305_key(k2, &nonce);
            let mac = Poly1305::new(&poly_key);
            let computed_tag = mac.compute_unpadded(ciphertext);
            if computed_tag.as_slice().ct_eq(tag).into() {
                // Decrypt payload (skip the 4-byte length prefix, start at counter=1)
                let mut cipher = make_chacha(k2, &nonce);
                // Seek past block 0 (the Poly1305 key derivation block)
                cipher.seek(64u32);
                #[allow(clippy::indexing_slicing)]
                cipher.apply_keystream(&mut ciphertext[super::super::PACKET_LENGTH_LEN..]);
                Ok(ciphertext)
            } else {
                Err(Error::DecryptionError)
            }
        }
    }

    impl super::super::SealingKey for SealingKey {
        fn padding_length(&self, payload: &[u8]) -> usize {
            let block_size = 8;
            let extra_len = super::super::PACKET_LENGTH_LEN + super::super::PADDING_LENGTH_LEN;
            let padding_len = if payload.len() + extra_len <= super::super::MINIMUM_PACKET_LEN {
                super::super::MINIMUM_PACKET_LEN - payload.len() - super::super::PADDING_LENGTH_LEN
            } else {
                block_size - ((super::super::PADDING_LENGTH_LEN + payload.len()) % block_size)
            };
            if padding_len < super::super::PACKET_LENGTH_LEN {
                padding_len + block_size
            } else {
                padding_len
            }
        }

        fn fill_padding(&self, padding_out: &mut [u8]) {
            for padding_byte in padding_out {
                *padding_byte = 0;
            }
        }

        fn tag_len(&self) -> usize {
            TAG_LEN
        }

        fn seal(
            &mut self,
            sequence_number: u32,
            plaintext_in_ciphertext_out: &mut [u8],
            tag_out: &mut [u8],
        ) {
            let (k2, k1) = split_key(&self.0);
            let nonce = make_nonce(sequence_number);

            // Encrypt the packet length with K1
            let mut length_cipher = make_chacha(k1, &nonce);
            #[allow(clippy::indexing_slicing)]
            length_cipher.apply_keystream(
                &mut plaintext_in_ciphertext_out[..super::super::PACKET_LENGTH_LEN],
            );

            // Encrypt payload with K2 at counter=1 (skip block 0 used for poly key)
            let mut payload_cipher = make_chacha(k2, &nonce);
            payload_cipher.seek(64u32);
            #[allow(clippy::indexing_slicing)]
            payload_cipher.apply_keystream(
                &mut plaintext_in_ciphertext_out[super::super::PACKET_LENGTH_LEN..],
            );

            // Compute Poly1305 tag over the full ciphertext (length + payload)
            let poly_key = derive_poly1305_key(k2, &nonce);
            let mac = Poly1305::new(&poly_key);
            let tag = mac.compute_unpadded(plaintext_in_ciphertext_out);
            tag_out.copy_from_slice(tag.as_slice());
        }
    }
}

#[cfg(all(feature = "rustcrypto", not(any(feature = "ring", feature = "aws-lc-rs"))))]
pub use pure_rust::SshChacha20Poly1305Cipher;
