"""RE:DEX's interface sounds, made from nothing but sine waves, a little filtered noise and envelopes (no samples, no
third-party sound), so they are ours to give away: CC0 (see assets/sounds/LICENSE).

    python make_ui_sounds.py <out dir>

writes click.wav, open.wav, close.wav, confirm.wav, refuse.wav, toast.wav: 48 kHz, 16-bit mono, a few kB each.
The same seed makes the same files, byte for byte.
"""
import math
import os
import random
import struct
import sys
import wave

RATE = 48000


def env(t, attack, decay):
    """a quick rise, then an exponential fall"""
    if t < attack:
        return t / attack
    return math.exp(-(t - attack) / decay)


def tone(f, t, shimmer=0.0):
    """a soft glassy tone: the fundamental, a quiet octave and a touch of the fifth above it"""
    w = 2 * math.pi * f * t
    return math.sin(w) + 0.25 * math.sin(2 * w + shimmer) + 0.08 * math.sin(3 * w)


def render(length, voice):
    n = int(RATE * length)
    out = [voice(i / RATE) for i in range(n)]
    peak = max(1e-9, max(abs(x) for x in out))
    # a 4 ms fade at the end so nothing clicks
    tail = int(RATE * 0.004)
    for i in range(tail):
        out[n - 1 - i] *= i / tail
    return [x / peak for x in out]


def noise_burst(seed, length, cutoff):
    """white noise through a one-pole low-pass: the 'tap' of glass"""
    rnd = random.Random(seed)
    a = math.exp(-2 * math.pi * cutoff / RATE)
    y, out = 0.0, []
    for _ in range(int(RATE * length)):
        y = a * y + (1 - a) * rnd.uniform(-1, 1)
        out.append(y)
    return out


def click():
    tap = noise_burst(1, 0.03, 5000)
    return render(0.07, lambda t: 0.55 * tone(1760, t) * env(t, 0.001, 0.012) + 2.5 * (tap[int(t * RATE)] if t < 0.03 else 0) * env(t, 0.0005, 0.006))


def chime(notes, gap, decay, length):
    def v(t):
        s = 0.0
        for k, f in enumerate(notes):
            start = k * gap
            if t >= start:
                s += tone(f, t - start, 0.6) * env(t - start, 0.004, decay)
        return s
    return render(length, v)


def refuse():
    # two low, slightly sour blips
    return render(0.22, lambda t: (math.sin(2 * math.pi * 196 * t) + 0.5 * math.sin(2 * math.pi * 207 * t)) * (env(t, 0.003, 0.03) + (env(t - 0.1, 0.003, 0.04) if t > 0.1 else 0)))


SOUNDS = {
    'click': click,
    # E6 then A6: up, for something opening
    'open': lambda: chime([1318.5, 1760.0], 0.045, 0.07, 0.26),
    # A6 then E6: down, for something closing
    'close': lambda: chime([1760.0, 1318.5], 0.04, 0.05, 0.2),
    # a bright major arpeggio, for Play and the main actions
    'confirm': lambda: chime([1046.5, 1318.5, 1568.0, 2093.0], 0.035, 0.12, 0.5),
    'refuse': refuse,
    # one soft high ping
    'toast': lambda: chime([2349.3], 0.0, 0.09, 0.22),
}


def write(path, samples, level):
    with wave.open(path, 'wb') as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(b''.join(struct.pack('<h', int(max(-1.0, min(1.0, s * level)) * 32767)) for s in samples))


def main():
    if len(sys.argv) != 2:
        sys.exit('usage: make_ui_sounds.py <out dir>')
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    for name, make in SOUNDS.items():
        write(os.path.join(out, name + '.wav'), make(), 0.7)
        print('made', name)


if __name__ == '__main__':
    main()
