# MP3 export for DJs

Bounce Mix and Bounce Stems write MP3 with SoundCraft's own pure-Rust encoder (MPEG-1 Layer III,
from [PR #607](https://github.com/storytold/soundcraft/pull/607) by Milkshak3s). In the Bounce
Mix window pick **MP3** as the format; from scripts pass `"format": "mp3"` and, optionally,
`"bitrate"` (32 to 320 kbps; default 320).

The window also offers:

- **Sample Rate**, 44.1 or 48 kHz (44.1 by default). The session is converted while it is encoded.
  From scripts: `"sample_rate": 44100` (or 48000, 32000). Without it the session rate is kept, or
  the nearest rate MP3 holds.
- **Title**, **Artist** and **BPM**, written as an ID3v2.3 tag at the head of the file (v2.3
  because older players read it more reliably than v2.4). From scripts: `"title"`, `"artist"`,
  `"bpm"` (1 to 999). Text beyond Latin-1 is stored as UTF-16. The tag sits before the `Info`
  frame, so the gapless timing is unchanged.
- A warning when the file already exists: **Bounce** becomes **Replace**, and only that second
  press writes over it.

## What CDJs need, and what SoundCraft writes

| Player | MP3 it plays |
|---|---|
| CDJ-2000NXS2 | MPEG-1 Layer III, 32 to 320 kbps, CBR or VBR, 32/44.1/48 kHz |
| CDJ-3000, CDJ-3000X | MP3 at 44.1/48 kHz |

SoundCraft writes:

- **320 kbps constant bitrate**, joint stereo, at 44.1 or 48 kHz (other session rates are
  resampled). Constant bitrate avoids the grid and seek problems DJs report with VBR files.
- **A gapless header** (`Info` with the `LAME` extension): the encoder delay (528 samples) and the
  end padding, with both CRCs. Decoders that read it start the music on the first sample and end
  on the last, so beat grids and cue points do not shift by the usual ~25 ms. Checked with
  ffmpeg and Apple's decoder: decoded length and timing match the input exactly, at 44.1 and
  48 kHz. The tag uses LAME's documented layout and version marker because decoders only read the
  delay from a tag marked that way.

WAV bounces of mono and stereo audio at 16 or 24 bits now use the plain PCM header. Some CDJs up to
the CDJ-3000 are reported to refuse WAV files with the WAVE_FORMAT_EXTENSIBLE header, which
SoundCraft used to write for 24-bit.

## Quality and what is still missing

On a 30-second excerpt of a reggaeton track at 320 kbps, the encoder measured within 0.5 dB of
LAME's signal-to-noise ratio, at about 135x realtime. Its known gap is short blocks: on isolated,
very sharp hits it leaves about 15 dB more noise in the 10 ms before each hit (pre-echo) than
LAME. In a full mix this is masked; in a drum-only intro it can be audible. Short-block switching
is the next step.

Not yet done: the key and comment ID3 fields (see
[PR #159](https://github.com/storytold/soundcraft/pull/159)), a bitrate picker in the window (the
command takes any bitrate), and a test on real CDJ hardware.
