"""Edit PC box slots in a copy of a Pokemon X/Y save (`main`, 0x65600 bytes) for test runs.

    python -I tools/save6/pk6_box.py IN OUT [--swap B:S,B:S ...] [--exp B:S=EXP ...] [--move1 B:S=K ...] [--list]
        [--party S=EXP,LEVEL,HP/ATK/DEF/SPA/SPD/SPE ...]

Boxes and slots count from 1, as the game shows them.
--swap exchanges two box slots; --exp sets a box Pokemon's experience (its level follows from EXP when the PC moves it
into the party, where the game computes the stats); --move1 swaps move K into the first move slot. PK6 data is decrypted, edited, re-checksummed and re-encrypted,
and every save block's CRC16 is rewritten. IN is only read; refuse to write over it.
--party sets party slot S's experience and level and recomputes its stored stats (Gen 6 formulas, from the species'
base stats given here, the PK6's IVs, EVs and nature): a party Pokemon's stats are stored, not recomputed by the game.
Its HP is set to full.
Layout (X/Y): boxes at 0x22600 (31 x 30 x 232 bytes), party at 0x14200, block table at the end ("BEEF").
"""
import struct, sys

BOX, SLOT_SIZE = 0x22600, 232
PARTY, PARTY_SIZE = 0x14200, 260
BP = [0,1,2,3, 0,1,3,2, 0,2,1,3, 0,3,1,2, 0,2,3,1, 0,3,2,1,
      1,0,2,3, 1,0,3,2, 2,0,1,3, 3,0,1,2, 2,0,3,1, 3,0,2,1,
      1,2,0,3, 1,3,0,2, 2,1,0,3, 3,1,0,2, 2,3,0,1, 3,2,0,1,
      1,2,3,0, 1,3,2,0, 2,1,3,0, 3,1,2,0, 2,3,1,0, 3,2,1,0]


def crypt(b, ec):
    b, seed = bytearray(b), ec
    for i in range(8, len(b), 2):
        if i == SLOT_SIZE:
            seed = ec  # a party slot's stats (232..260) have a stream of their own, from the same seed
        seed = (seed * 0x41C64E6D + 0x6073) & 0xFFFFFFFF
        struct.pack_into('<H', b, i, struct.unpack_from('<H', b, i)[0] ^ (seed >> 16))
    return b


def shuffle(b, order):
    out = bytearray(b)
    for i in range(4):
        out[8 + 56 * i: 64 + 56 * i] = b[8 + 56 * order[i]: 64 + 56 * order[i]]
    return out


def sv(ec):
    return ((ec >> 13) & 31) % 24


def decrypt(ek):
    ec = struct.unpack_from('<I', ek)[0]
    return shuffle(crypt(ek, ec), BP[sv(ec) * 4: sv(ec) * 4 + 4])


def encrypt(pk):
    ec = struct.unpack_from('<I', pk)[0]
    order = BP[sv(ec) * 4: sv(ec) * 4 + 4]
    inverse = [order.index(i) for i in range(4)]
    struct.pack_into('<H', pk, 6, sum(struct.unpack_from('<112H', pk, 8)) & 0xFFFF)
    return crypt(shuffle(pk, inverse), ec)


def stats(pk, level, base):
    """Gen 6 stats (HP, Atk, Def, Spe, SpA, SpD) for this PK6 at `level`; `base` is HP, Atk, Def, SpA, SpD, Spe."""
    hp, atk, de, spa, spd, spe = base
    b = (hp, atk, de, spe, spa, spd)
    ivs = struct.unpack_from('<I', pk, 0x74)[0]
    iv = [(ivs >> (5 * i)) & 31 for i in range(6)]  # HP, Atk, Def, Spe, SpA, SpD
    ev = list(pk[0x1E:0x24])
    nature = pk[0x1C]
    up, down = nature // 5, nature % 5  # over Atk, Def, Spe, SpA, SpD
    out = [(2 * b[0] + iv[0] + ev[0] // 4) * level // 100 + level + 10]
    for i in range(1, 6):
        v = (2 * b[i] + iv[i] + ev[i] // 4) * level // 100 + 5
        mul = 1.1 if up == i - 1 and up != down else 0.9 if down == i - 1 and up != down else 1.0
        out.append(int(v * mul))
    return out


def checksum_ok(pk):
    return sum(struct.unpack_from('<112H', pk, 8)) & 0xFFFF == struct.unpack_from('<H', pk, 6)[0]


def crc16(b):
    c = 0xFFFF
    for x in b:
        c ^= x << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) & 0xFFFF if c & 0x8000 else (c << 1) & 0xFFFF
    return c


def blocks(d):
    """(table offset of the entry, block offset, length) for each save block."""
    o = len(d) - 0x200 + 0x10
    if struct.unpack_from('<I', d, o)[0] != 0x42454546:
        o -= 0x200
    assert struct.unpack_from('<I', d, o)[0] == 0x42454546, 'no BEEF block table: not an X/Y save?'
    off, out = 0, []
    for i in range((len(d) - o - 8) // 8):
        e = o + 4 + 8 * i
        ln = struct.unpack_from('<I', d, e)[0]
        if ln == 0:
            break
        out.append((e, off, ln))
        off = (off + ln + 0x1FF) & ~0x1FF
    return out


def slot(spec):
    b, s = (int(x) for x in spec.split(':'))
    assert 1 <= b <= 31 and 1 <= s <= 30, spec
    return BOX + SLOT_SIZE * ((b - 1) * 30 + s - 1)


def main(argv):
    src, dst, ops = argv[0], argv[1], argv[2:]
    assert src != dst, 'write to a copy, never over the input'
    d = bytearray(open(src, 'rb').read())
    assert len(d) == 0x65600, f'{len(d):#x} bytes: not an X/Y main save'
    table = blocks(d)
    for e, off, ln in table:
        assert crc16(d[off:off + ln]) == struct.unpack_from('<H', d, e + 6)[0], f'block at {off:#x}: bad CRC in the input'
    while ops:
        op = ops.pop(0)
        arg = ops.pop(0) if op != '--list' else ''
        if op == '--swap':
            a, b = (slot(x) for x in arg.split(','))
            d[a:a + SLOT_SIZE], d[b:b + SLOT_SIZE] = d[b:b + SLOT_SIZE], d[a:a + SLOT_SIZE]
        elif op == '--exp':
            spec, exp = arg.split('=')
            o = slot(spec)
            pk = decrypt(d[o:o + SLOT_SIZE])
            assert checksum_ok(pk) and struct.unpack_from('<H', pk, 8)[0], f'{spec}: empty or bad slot'
            struct.pack_into('<I', pk, 0x10, int(exp))
            enc = encrypt(pk)
            assert decrypt(enc) == pk
            d[o:o + SLOT_SIZE] = enc
        elif op == '--move1':
            # B:S=K: move K (with its PP and PP Ups) becomes the first move, as the summary screen's reordering does;
            # a scripted battle's A on the move screen then picks it too
            spec, k = arg.split('=')
            o, k = slot(spec), int(k) - 1
            assert 0 <= k < 4, arg
            pk = decrypt(d[o:o + SLOT_SIZE])
            assert checksum_ok(pk) and struct.unpack_from('<H', pk, 8)[0], f'{spec}: empty or bad slot'
            for base, size in ((0x5A, 2), (0x62, 1), (0x66, 1)):
                a, b = base, base + size * k
                pk[a:a + size], pk[b:b + size] = pk[b:b + size], pk[a:a + size]
            enc = encrypt(pk)
            assert decrypt(enc) == pk
            d[o:o + SLOT_SIZE] = enc
        elif op == '--party':
            spec, rest = arg.split('=')
            exp, level, base = rest.split(',')
            n = int(spec)
            assert 1 <= n <= 6, spec
            o = PARTY + PARTY_SIZE * (n - 1)
            pk = decrypt(d[o:o + PARTY_SIZE])
            assert checksum_ok(pk) and struct.unpack_from('<H', pk, 8)[0], f'party {n}: empty or bad slot'
            st = stats(pk, int(level), [int(x) for x in base.split('/')])
            struct.pack_into('<I', pk, 0x10, int(exp))
            pk[0xEC] = int(level)
            struct.pack_into('<7H', pk, 0xF0, st[0], *st)  # HP now, then HP, Atk, Def, Spe, SpA, SpD
            enc = encrypt(pk)
            assert decrypt(enc) == pk
            d[o:o + PARTY_SIZE] = enc
            print(f'party {n}: species {struct.unpack_from("<H", pk, 8)[0]} level {level} exp {exp} stats {st}')
        elif op == '--list':
            for i in range(31 * 30):
                o = BOX + SLOT_SIZE * i
                pk = decrypt(d[o:o + SLOT_SIZE])
                species, exp = struct.unpack_from('<H', pk, 8)[0], struct.unpack_from('<I', pk, 0x10)[0]
                if species and checksum_ok(pk):
                    print(f'{i // 30 + 1}:{i % 30 + 1} species {species} exp {exp} moves {struct.unpack_from("<4H", pk, 0x5A)}')
        else:
            sys.exit(f'unknown option {op}')
    for e, off, ln in table:
        struct.pack_into('<H', d, e + 6, crc16(d[off:off + ln]))
    open(dst, 'wb').write(d)


if __name__ == '__main__':
    main(sys.argv[1:])
