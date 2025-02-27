// Copyright 2016 Brian Smith.
// Portions Copyright (c) 2016, Google Inc.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use super::BLOCK_LEN;
use super::{quic::Sample, Nonce};
use crate::polyfill::ChunksFixed;

#[cfg(any(
    test,
    not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86",
        target_arch = "x86_64",
    ))
))]
mod fallback;

#[repr(transparent)]
pub struct Key([u32; KEY_LEN / 4]);

impl From<[u8; KEY_LEN]> for Key {
    #[inline]
    fn from(value: [u8; KEY_LEN]) -> Self {
        let value = value.chunks_fixed();
        Self([
            u32::from_le_bytes(value[0]),
            u32::from_le_bytes(value[1]),
            u32::from_le_bytes(value[2]),
            u32::from_le_bytes(value[3]),
            u32::from_le_bytes(value[4]),
            u32::from_le_bytes(value[5]),
            u32::from_le_bytes(value[6]),
            u32::from_le_bytes(value[7]),
        ])
    }
}

impl Key {
    #[inline] // Optimize away match on `counter`.
    pub fn encrypt_in_place(&self, counter: Counter, in_out: &mut [u8]) {
        unsafe {
            self.encrypt(
                CounterOrIv::Counter(counter),
                in_out.as_ptr(),
                in_out.len(),
                in_out.as_mut_ptr(),
            );
        }
    }

    #[inline] // Optimize away match on `iv` and length check.
    pub fn encrypt_iv_xor_blocks_in_place(&self, iv: Iv, in_out: &mut [u8; 2 * BLOCK_LEN]) {
        unsafe {
            self.encrypt(
                CounterOrIv::Iv(iv),
                in_out.as_ptr(),
                in_out.len(),
                in_out.as_mut_ptr(),
            );
        }
    }

    #[inline]
    pub fn new_mask(&self, sample: Sample) -> [u8; 5] {
        let mut out: [u8; 5] = [0; 5];
        let iv = Iv::assume_unique_for_key(sample);

        unsafe {
            self.encrypt(
                CounterOrIv::Iv(iv),
                out.as_ptr(),
                out.len(),
                out.as_mut_ptr(),
            );
        }

        out
    }

    pub fn encrypt_overlapping(&self, counter: Counter, in_out: &mut [u8], in_prefix_len: usize) {
        // XXX: The x86 and at least one branch of the ARM assembly language
        // code doesn't allow overlapping input and output unless they are
        // exactly overlapping. TODO: Figure out which branch of the ARM code
        // has this limitation and come up with a better solution.
        //
        // https://rt.openssl.org/Ticket/Display.html?id=4362
        let len = in_out.len() - in_prefix_len;
        if cfg!(any(target_arch = "arm", target_arch = "x86")) && in_prefix_len != 0 {
            in_out.copy_within(in_prefix_len.., 0);
            self.encrypt_in_place(counter, &mut in_out[..len]);
        } else {
            unsafe {
                self.encrypt(
                    CounterOrIv::Counter(counter),
                    in_out[in_prefix_len..].as_ptr(),
                    len,
                    in_out.as_mut_ptr(),
                );
            }
        }
    }

    #[inline] // Optimize away match on `counter.`
    unsafe fn encrypt(
        &self,
        counter: CounterOrIv,
        input: *const u8,
        in_out_len: usize,
        output: *mut u8,
    ) {
        #[cfg(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86",
            target_arch = "x86_64",
        ))]
        #[inline(always)]
        fn chacha20_ctr32(
            key: &Key,
            counter: CounterOrIv,
            input: *const u8,
            in_out_len: usize,
            output: *mut u8,
        ) {
            let iv = match counter {
                CounterOrIv::Counter(counter) => counter.into(),
                CounterOrIv::Iv(iv) => {
                    assert!(in_out_len <= 32);
                    iv
                }
            };
            // There's no need to worry if `counter` is incremented because it is
            // owned here and we drop immediately after the call.
            extern "C" {
                fn GFp_ChaCha20_ctr32(
                    out: *mut u8,
                    in_: *const u8,
                    in_len: crate::c::size_t,
                    key: &Key,
                    first_iv: &Iv,
                );
            }
            unsafe { GFp_ChaCha20_ctr32(output, input, in_out_len, key, &iv) }
        }

        #[cfg(not(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86",
            target_arch = "x86_64",
        )))]
        use fallback::chacha20_ctr32;

        chacha20_ctr32(self, counter, input, in_out_len, output);
    }

    #[cfg(any(
        test,
        not(any(target_arch = "aarch64", target_arch = "arm", target_arch = "x86"))
    ))]
    #[inline]
    pub(super) fn words_less_safe(&self) -> &[u32; KEY_LEN / 4] {
        &self.0
    }
}

/// Counter || Nonce, all native endian.
pub struct Counter([u32; 4]);

impl Counter {
    pub fn zero(nonce: Nonce) -> Self {
        Self::from_nonce_and_ctr(nonce, 0)
    }

    fn from_nonce_and_ctr(nonce: Nonce, ctr: u32) -> Self {
        let nonce = nonce.as_ref().chunks_fixed();
        Self([
            ctr,
            u32::from_le_bytes(nonce[0]),
            u32::from_le_bytes(nonce[1]),
            u32::from_le_bytes(nonce[2]),
        ])
    }

    pub fn increment(&mut self) -> Iv {
        let iv = Iv(self.0);
        self.0[0] += 1;
        iv
    }

    /// This is "less safe" because it hands off management of the counter to
    /// the caller.
    #[cfg(any(
        test,
        not(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86",
            target_arch = "x86_64",
        ))
    ))]
    fn into_words_less_safe(self) -> [u32; 4] {
        self.0
    }
}

/// The IV for a single block encryption.
///
/// Intentionally not `Clone` to ensure each is used only once.
#[repr(transparent)]
pub struct Iv([u32; 4]);

impl Iv {
    fn assume_unique_for_key(value: [u8; 16]) -> Self {
        let value: &[[u8; 4]; 4] = value.chunks_fixed();
        Self([
            u32::from_le_bytes(value[0]),
            u32::from_le_bytes(value[1]),
            u32::from_le_bytes(value[2]),
            u32::from_le_bytes(value[3]),
        ])
    }

    /// This is "less safe" because it hands off management of the counter to
    /// the caller.
    #[cfg(any(
        test,
        not(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86",
            target_arch = "x86_64",
        ))
    ))]
    fn into_words_less_safe(self) -> [u32; 4] {
        self.0
    }
}

impl From<Counter> for Iv {
    fn from(counter: Counter) -> Self {
        Self(counter.0)
    }
}

enum CounterOrIv {
    Counter(Counter),
    Iv(Iv),
}

const KEY_BLOCKS: usize = 2;
pub const KEY_LEN: usize = KEY_BLOCKS * BLOCK_LEN;

#[cfg(test_not_for_now)]
mod tests {
    use super::*;
    use crate::{polyfill, test};
    use alloc::vec;
    use core::convert::TryInto;

    const MAX_ALIGNMENT_AND_OFFSET: (usize, usize) = (15, 259);
    const MAX_ALIGNMENT_AND_OFFSET_SUBSET: (usize, usize) =
        if cfg!(any(debug_assertions = "false", feature = "slow_tests")) {
            MAX_ALIGNMENT_AND_OFFSET
        } else {
            (0, 0)
        };

    #[test]
    fn chacha20_test_default() {
        // Always use `MAX_OFFSET` if we hav assembly code.
        let max_offset = if cfg!(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86",
            target_arch = "x86_64"
        )) {
            MAX_ALIGNMENT_AND_OFFSET
        } else {
            MAX_ALIGNMENT_AND_OFFSET_SUBSET
        };
        chacha20_test(max_offset, Key::encrypt_within);
    }

    // Smoketest the fallback implementation.
    #[test]
    fn chacha20_test_fallback() {
        chacha20_test(MAX_ALIGNMENT_AND_OFFSET_SUBSET, fallback::chacha20_ctr32);
    }

    // Verifies the encryption is successful when done on overlapping buffers.
    //
    // On some branches of the 32-bit x86 and ARM assembly code the in-place
    // operation fails in some situations where the input/output buffers are
    // not exactly overlapping. Such failures are dependent not only on the
    // degree of overlapping but also the length of the data. `encrypt_within`
    // works around that.
    fn chacha20_test(
        max_alignment_and_offset: (usize, usize),
        f: impl for<'k, 'i> Fn(&'k Key, Counter, &'i mut [u8], RangeFrom<usize>),
    ) {
        // Reuse a buffer to avoid slowing down the tests with allocations.
        let mut buf = vec![0u8; 1300];

        test::run(test_file!("chacha_tests.txt"), move |section, test_case| {
            assert_eq!(section, "");

            let key = test_case.consume_bytes("Key");
            let key: &[u8; KEY_LEN] = key.as_slice().try_into()?;
            let key = Key::from(*key);

            let ctr = test_case.consume_usize("Ctr");
            let nonce = test_case.consume_bytes("Nonce");
            let input = test_case.consume_bytes("Input");
            let output = test_case.consume_bytes("Output");

            // Pre-allocate buffer for use in test_cases.
            let mut in_out_buf = vec![0u8; input.len() + 276];

            // Run the test case over all prefixes of the input because the
            // behavior of ChaCha20 implementation changes dependent on the
            // length of the input.
            for len in 0..(input.len() + 1) {
                chacha20_test_case_inner(
                    &key,
                    &nonce,
                    ctr as u32,
                    &input[..len],
                    &output[..len],
                    &mut buf,
                    max_alignment_and_offset,
                    &f,
                );
            }

            Ok(())
        });
    }

    fn chacha20_test_case_inner(
        key: &Key,
        nonce: &[u8],
        ctr: u32,
        input: &[u8],
        expected: &[u8],
        buf: &mut [u8],
        (max_alignment, max_offset): (usize, usize),
        f: &impl for<'k, 'i> Fn(&'k Key, Counter, &'i mut [u8], RangeFrom<usize>),
    ) {
        const ARBITRARY: u8 = 123;

        let counter =
            Counter::from_nonce_and_ctr(Nonce::try_assume_unique_for_key(nonce).unwrap(), ctr);

        for alignment in 0..=max_alignment {
            polyfill::slice::fill(&mut buf[..alignment], ARBITRARY);
            let buf = &mut buf[alignment..];
            for offset in 0..=max_offset {
                let buf = &mut buf[..(offset + input.len())];
                polyfill::slice::fill(&mut buf[..offset], ARBITRARY);
                let src = offset..;
                buf[src.clone()].copy_from_slice(input);

                let ctr = Counter::from_nonce_and_ctr(
                    Nonce::try_assume_unique_for_key(nonce).unwrap(),
                    ctr,
                );
                f(key, ctr, buf, src);
                assert_eq!(&buf[..input.len()], expected)
            }
        }
    }
}
