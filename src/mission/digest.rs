use sha2::{Digest as _, Sha256};

/// Length-delimited, domain-separated SHA-256 input for mission authority data.
pub(crate) struct CanonicalDigest {
    hasher: Sha256,
}

impl CanonicalDigest {
    pub(crate) fn new(domain: &'static [u8]) -> Self {
        let mut digest = Self {
            hasher: Sha256::new(),
        };
        digest.bytes(b"mission-authority-canonical-v1");
        digest.bytes(domain);
        digest
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.hasher.update((value.len() as u64).to_be_bytes());
        self.hasher.update(value);
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.bytes(&[u8::from(value)]);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes(&[value]);
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes(&value.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.bytes(&value.to_be_bytes());
    }

    pub(crate) fn finish(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = self.hasher.finalize();
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}
