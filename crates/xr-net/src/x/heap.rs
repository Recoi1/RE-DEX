//! X's heaps (nn::fnd ExpHeap, "EXPH" heads), read only: how much the field's model heaps have left, so puppets stop
//! before the game's own allocations fail (a failed one ends in the fatal loop). Layouts and evidence:
//! docs/re/x-field-move-models.md, "How many".
use crate::Guest;

/// The first heap head in linear memory (the root of its heap tree).
pub const LINEAR_ROOT: u32 = 0x1400_0038;
/// "EXPH" little-endian.
const SIG: u32 = 0x4558_5048;
/// The gfl heap ids of the two 1 MB heaps a puppet's model instance comes from (Laverre Pokemon Center and street,
/// Lumiose: see the doc).
pub const MODEL_HEAPS: [u32; 2] = [0x14B, 0x14C];

/// Head: +0x04/+0x08 sibling links, +0x0C first child, +0x24 first free block, +0x50 the gfl heap id.
/// Free block: u16 "FR" (0x4652), +0x04 size (after the 16-byte header), +0x0C next free block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Free {
    pub total: u32,
    pub largest: u32,
}

/// The free space of each heap with these ids, in `ids` order; None when they are not all found (another build or
/// layout).
pub fn free(g: &mut impl Guest, ids: &[u32]) -> Option<Vec<Free>> {
    let mut stack = vec![LINEAR_ROOT];
    let (mut out, mut seen) = (vec![None::<Free>; ids.len()], 0);
    while let Some(h) = stack.pop() {
        seen += 1;
        if seen > 4096 || g.u32(h)? != SIG {
            return None;
        }
        let (next, child) = (g.u32(h.checked_add(8)?)?, g.u32(h.checked_add(0xC)?)?);
        let id = g.u32(h.checked_add(0x50)?)?;
        if let Some(i) = ids.iter().position(|&x| x == id) {
            let out = out[i].insert(Free { total: 0, largest: 0 });
            let mut b = g.u32(h.checked_add(0x24)?)?;
            for _ in 0..100_000 {
                if b == 0 {
                    break;
                }
                if g.u16(b)? != 0x4652 {
                    return None;
                }
                let size = g.u32(b.checked_add(4)?)?;
                out.total = out.total.saturating_add(size);
                out.largest = out.largest.max(size);
                b = g.u32(b.checked_add(0xC)?)?;
            }
        }
        // the root's own siblings are the other memory regions' roots: only its subtree
        for n in [next, child] {
            if n != 0 && !(h == LINEAR_ROOT && n == next) {
                stack.push(n);
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Words(std::collections::HashMap<u32, u8>);
    impl Guest for Words {
        fn read(&mut self, addr: u32, out: &mut [u8]) -> bool {
            for (i, b) in out.iter_mut().enumerate() {
                *b = *self.0.get(&(addr + i as u32)).unwrap_or(&0);
            }
            true
        }
    }
    impl Words {
        fn put(&mut self, a: u32, v: u32) {
            for (i, b) in v.to_le_bytes().iter().enumerate() {
                self.0.insert(a + i as u32, *b);
            }
        }
        fn head(&mut self, a: u32, next: u32, child: u32, id: u32, free: u32) {
            self.put(a, SIG);
            self.put(a + 8, next);
            self.put(a + 0xC, child);
            self.put(a + 0x50, id);
            self.put(a + 0x24, free);
        }
        fn block(&mut self, a: u32, size: u32, next: u32) {
            self.put(a, 0x4652);
            self.put(a + 4, size);
            self.put(a + 0xC, next);
        }
    }

    #[test]
    fn reads_each_model_heap() {
        let mut g = Words(Default::default());
        // root -> child A (id 0x5D) -> its children 0x14B, 0x14C (siblings); another root beside the linear one
        g.head(LINEAR_ROOT, 0x0880_3038, 0x1400_0100, 1, 0);
        g.head(0x1400_0100, 0, 0x14CE_31D8, 0x5D, 0);
        g.head(0x14CE_31D8, 0x14DE_3208, 0, 0x14B, 0x14D2_A744);
        g.head(0x14DE_3208, 0, 0, 0x14C, 0x14DF_4DF4);
        g.block(0x14D2_A744, 756_300, 0x14D2_0000);
        g.block(0x14D2_0000, 44, 0);
        g.block(0x14DF_4DF4, 648_140, 0);
        assert_eq!(free(&mut g, &MODEL_HEAPS), Some(vec![Free { total: 756_344, largest: 756_300 }, Free { total: 648_140, largest: 648_140 }]));
        // a heap missing, or a broken free list: no answer rather than a wrong one
        assert_eq!(free(&mut g, &[0x14B, 0x999]), None);
        g.put(0x14D2_0000, 0);
        assert_eq!(free(&mut g, &MODEL_HEAPS), None);
    }
}
