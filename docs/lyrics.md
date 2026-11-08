# Lyrics and chorus

SoundCraft can write a song's lyrics into the **Lyrics** ruler above the tracks and mark the
chorus there. Transcription runs on this computer with NVIDIA's
[Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) on the GPU (through
[parakeet-rs](https://github.com/altunenes/parakeet-rs) and Burn). It handles 25 European
languages, including Spanish and English, and detects the language on its own. Nothing is
uploaded.

## In the app

1. Select an audio clip.
2. Choose **AudioSuite › Transcribe Lyrics…**, or right-click the clip and choose
   **Transcribe Lyrics…**.
3. Keep **Separate the vocals first** on for a full song: the vocal stem is transcribed instead
   of the mix, which avoids most mistakes from the music. A clip on a "Vocals - …" track from
   Stem Separation is transcribed as it is.
4. Click **Transcribe**. The first time, the button reads **Download 2.6 GB & Transcribe**: the
   speech model (2.55 GB) and, if needed, the stem model (85 MB) are downloaded into the
   settings folder (`Models`) and checked against their SHA-256.

The progress card at the bottom right shows the job, with Cancel. When it finishes, the
**Lyrics** ruler appears (View › Rulers › Lyrics shows or hides it). Each line sits at its time,
and the chorus lines are orange. Hover a line to read all of it. Click a line to select its time
on every audio track.

Right-click the Lyrics ruler for:

| Item | What it does |
|---|---|
| Highlight Chorus | Turns the orange chorus colour on or off (on by default; the Transcribe Lyrics window has the same option). |
| Chorus *n* › Select on All Audio Tracks | Selects that chorus on every audio track, ready to copy. |
| Chorus *n* › Copy Lyrics to Insertion Point | Copies that chorus's lines to the edit insertion, for example onto a chorus you pasted as an intro. |
| Clear Lyrics | Removes the lyrics. |
| Hide Lyrics Ruler | Hides the ruler; the lyrics stay. |

Lyrics are saved with the session. Insert Time and Cut Time move them with the music.

## How the chorus is found

1. The words are grouped into lines at pauses (sung lyrics have little punctuation).
2. Lines that say nearly the same thing are grouped, ignoring case, accents and ad-libs in
   parentheses.
3. The block of 2 to 8 consecutive lines that repeats most often is the chorus.
4. The vocal melody of that block is then compared with the whole vocal stem, which finds
   repeats that were transcribed differently. The vocal is used rather than the mix because in
   reggaeton and similar genres the backing loop often runs through the whole song.

In COSITA (Daddy Yankee and Cardi B, 3:10), this found the chorus four times. Transcription took
about 4.5 s once the model was loaded (the first run takes about 20 s longer while the GPU
kernels are tuned), or about 42 s for the first run including vocal separation.

Expect mistakes in the words: sung, Auto-Tuned and slang-heavy lyrics are hard for any speech
model, and ad-libs and backing vocals are often missed. The chorus usually survives this,
because a repeated chorus tends to be transcribed the same way each time.

## From scripts

| Command | Params | What it does |
|---|---|---|
| `lyrics.models` | `{}` | The speech model, its size and terms, whether it is installed, and the models folder. |
| `lyrics.install_model` | `{model?}` | Downloads and verifies the speech model. Blocks until done. |
| `lyrics.transcribe` | `{clip?, separate?, model?}` | Transcribes and stores the lyrics as one undo step. Blocks until done. |
| `lyrics.inspect` | `{}` | Every line (`start`, `end`, `start_secs`, `end_secs`, `text`, `chorus`), the `choruses` (each occurrence) and `sections` (occurrences less than 8 s apart merged). |
| `lyrics.select_chorus` | `{index?, section?, tracks?}` | Selects a chorus (default the first) on every audio track or the given ones. |
| `lyrics.copy` | `{start, end, to}` | Copies the lines that start in `start..end` to `to`. |
| `lyrics.highlight_chorus` | `{value?}` | Turns the chorus highlight on or off. |
| `lyrics.clear` | `{}` | Removes the lyrics. |
| `view.ruler_lyrics` | `{visible?}` | Shows or hides the Lyrics ruler. |
| `lyrics.start` | `lyrics.transcribe` params, plus `download?` | App only. Starts a background job (see `jobs.status`). |
| `lyrics.download_model` | `{model?}` | App only. Downloads the speech model in the background. |

### Example: the chorus as an intro

Run against the app's control server, this puts the first chorus (vocals and every stem) in
front of the song:

1. `stems.start {"clips": [3]}` and wait for the job, so the song has stem tracks.
2. `lyrics.start {"clip": 3}` and wait, then `lyrics.inspect` for `choruses[0]`.
3. Snap the chorus to whole bars of the song's tempo (`start` on a downbeat, length 4 or 8 bars).
4. `edit.copy {"tracks": [every audio track], "start": S, "end": E}`.
5. `event.insert_time {"start": 0, "length": E - S}`: the song, the stems and the lyrics move back.
6. `edit.paste {"tracks": [the same tracks], "at": 0}`.
7. `lyrics.copy {"start": S + (E - S), "end": E + (E - S), "to": 0}` so the intro shows its words.

On COSITA this made a 4-bar (9.6 s) intro that is sample-identical to the chorus in the song.

## Model terms

Parakeet TDT 0.6B v3 is by NVIDIA and licensed CC-BY-4.0. SoundCraft downloads the ONNX export
published for parakeet-rs only when you ask for it. The web build does not include
transcription.
