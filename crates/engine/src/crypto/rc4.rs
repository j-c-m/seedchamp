//! RC4 stream cipher (MSE payload crypto).
//!
//! Hot path is [`Rc4::crypt_inplace`] on every encrypted peer byte. PRGA keeps
//! `i`/`j` as `usize` and uses an explicit swap (faster than u8 + cast on the
//! 16 KiB PIECE path). Algorithm is classic ARC4 / BitTorrent MSE (1024-byte drop).

/// Classic RC4 (ARC4). Discard 1024 bytes after init for MSE.
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: usize,
    j: usize,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty());
        let mut s = [0u8; 256];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut j: usize = 0;
        let klen = key.len();
        for i in 0..256 {
            j = (j + s[i] as usize + key[i % klen] as usize) & 255;
            s.swap(i, j);
        }
        Self { s, i: 0, j: 0 }
    }

    /// MSE: discard first 1024 keystream bytes after key setup.
    pub fn new_mse(key: &[u8]) -> Self {
        let mut rc4 = Self::new(key);
        rc4.drop_keystream(1024);
        rc4
    }

    /// Advance the PRGA without XORing (faster than crypt of zeros).
    pub fn drop_keystream(&mut self, n: usize) {
        let s = &mut self.s;
        let mut i = self.i;
        let mut j = self.j;
        for _ in 0..n {
            i = (i + 1) & 255;
            j = (j + s[i] as usize) & 255;
            s.swap(i, j);
        }
        self.i = i;
        self.j = j;
    }

    /// Encrypt/decrypt `data` in place (keystream XOR).
    #[inline]
    pub fn crypt_inplace(&mut self, data: &mut [u8]) {
        let s = &mut self.s;
        let mut i = self.i;
        let mut j = self.j;
        for b in data.iter_mut() {
            i = (i + 1) & 255;
            j = (j + s[i] as usize) & 255;
            let si = s[i];
            let sj = s[j];
            s[i] = sj;
            s[j] = si;
            // After swap S[i]=sj, S[j]=si; t = (si+sj)&255 is correct for k.
            *b ^= s[(si as usize + sj as usize) & 255];
        }
        self.i = i;
        self.j = j;
    }

    pub fn crypt(&mut self, input: &[u8], output: &mut [u8]) {
        assert_eq!(input.len(), output.len());
        output.copy_from_slice(input);
        self.crypt_inplace(output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = b"secret-key";
        let mut enc = Rc4::new(key);
        let mut dec = Rc4::new(key);
        let mut msg = b"hello bitTorrent MSE".to_vec();
        let orig = msg.clone();
        enc.crypt_inplace(&mut msg);
        assert_ne!(msg, orig);
        dec.crypt_inplace(&mut msg);
        assert_eq!(msg, orig);
    }

    #[test]
    fn mse_discard_diverges() {
        let key = b"0123456789abcdef0123";
        let mut a = Rc4::new(key);
        let mut b = Rc4::new_mse(key);
        let mut x = [0u8; 16];
        let mut y = [0u8; 16];
        a.crypt_inplace(&mut x);
        b.crypt_inplace(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn drop_keystream_matches_crypt_zeros() {
        let key = b"0123456789abcdef0123";
        let mut via_drop = Rc4::new(key);
        via_drop.drop_keystream(1024);
        let mut via_crypt = Rc4::new(key);
        let mut z = [0u8; 1024];
        via_crypt.crypt_inplace(&mut z);

        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        via_drop.crypt_inplace(&mut a);
        via_crypt.crypt_inplace(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn bulk_roundtrip_16k() {
        let key = b"bulk-key-for-mse!!";
        let mut enc = Rc4::new_mse(key);
        let mut dec = Rc4::new_mse(key);
        let mut msg: Vec<u8> = (0..16 * 1024).map(|i| (i % 251) as u8).collect();
        let orig = msg.clone();
        enc.crypt_inplace(&mut msg);
        assert_ne!(msg, orig);
        dec.crypt_inplace(&mut msg);
        assert_eq!(msg, orig);
    }

    /// Reference PRGA for the golden test.
    fn ref_crypt(s: &mut [u8; 256], i: &mut u8, j: &mut u8, data: &mut [u8]) {
        for b in data.iter_mut() {
            *i = i.wrapping_add(1);
            *j = j.wrapping_add(s[*i as usize]);
            s.swap(*i as usize, *j as usize);
            let k = s[(s[*i as usize].wrapping_add(s[*j as usize])) as usize];
            *b ^= k;
        }
    }

    #[test]
    fn matches_reference_prga() {
        let key = b"compat-check-key";
        let mut opt = Rc4::new(key);
        let mut s = [0u8; 256];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut jr: u8 = 0;
        for i in 0..256 {
            jr = jr.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, jr as usize);
        }
        let mut i: u8 = 0;
        let mut j: u8 = 0;

        let mut a: Vec<u8> = (0..10_000).map(|x| (x % 256) as u8).collect();
        let mut b = a.clone();
        opt.crypt_inplace(&mut a);
        ref_crypt(&mut s, &mut i, &mut j, &mut b);
        assert_eq!(a, b);
    }
}
