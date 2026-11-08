# Stem separation

Stem separation splits a clip into its parts (vocals, drums, bass and other, or six stems with
guitar and piano) and puts each part on its own track. It runs on this computer with
[HTDemucs v4](https://arxiv.org/abs/2211.08553) on the GPU (Metal on macOS, Vulkan or DirectX
elsewhere, through wgpu). Nothing is uploaded.

## In the app

1. Select one or more audio clips.
2. Choose **AudioSuite › Stem Separation…**, or right-click a clip and choose **Stem Separation…**.
3. Pick a model and the stems you want, then click **Separate**. The first time, the button reads
   **Download 85 MB & Separate**: the model is downloaded into the settings folder (`Models`) and
   checked against its SHA-256 before it is used.

A card at the bottom right shows each job's progress, the time left and a Cancel button. The app
stays usable while it runs. When a job finishes, the stems land in a routing folder named
"*clip* Stems" right below the clip's track: one colour-coded stereo track per stem, each clip
aligned with the original and carrying its fades and clip gain. The original clip is muted (it
stays in place, so you can unmute it to compare). Undo removes everything in one step.

Options:

| Option | What it does |
|---|---|
| Model | **Standard** (`htdemucs`, 85 MB): fast, good quality. **Fine-tuned** (`htdemucs_ft`, 337 MB): best quality, about four times slower. **Six stems** (`htdemucs_6s`, 55 MB): adds guitar and piano; piano is weak. |
| Stems | Any of the model's stems. Fewer stems make fewer tracks; the model runs either way. |
| Instrumental | Adds a track holding the clip minus its vocal stem. Vocals plus Instrumental add back to the clip exactly. The four stems add back only approximately. |
| Mute the original clip | On by default. |

Timings on an Apple M-series laptop for a 3:10 song with the Standard model: about 14 s. The
first separation after installing a model takes about 20 s longer, while the GPU kernels are
tuned; the tuning is cached in `Models/gpu-cache`. The model stays loaded until another model is
used. One clip can be up to 20 minutes long.

The web build does not include stem separation.

## Model terms

The HTDemucs code is MIT-licensed. Its authors describe the pretrained weights as released for
research. SoundCraft does not bundle them: it downloads the safetensors conversion published by
the [demucs-rs](https://github.com/nikhilunni/demucs-rs) project only when you ask for it, and
the window shows these terms before the first download. Check them before commercial use.

## From scripts

| Command | Params | What it does |
|---|---|---|
| `stems.models` | `{}` | The models, their size, stems and terms, whether each is installed, and the models folder. |
| `stems.install_model` | `{model?}` | Downloads and verifies a model. Blocks until done. |
| `stems.separate` | `{clips?, model?, stems?, instrumental?, mute_original?}` | Separates and places the stems as one undo step. Blocks until done. Errors when the model is missing. |
| `stems.start` | same as `stems.separate`, plus `download?: bool` | App only. Starts a background job and returns `{job, waits_for}` at once. With `download: true` a missing model is downloaded first (`waits_for` is that download's job). |
| `stems.download_model` | `{model?}` | App only. Downloads a model in the background: `{job}`, or `{installed: true}`. |
| `jobs.list` | `{}` | Every recent job. |
| `jobs.status` | `{job}` | One job: `state` (`queued`, `running`, `done`, `failed`, `cancelled`), `progress` (0–1), `seconds`, and `result` or `error`. |
| `jobs.cancel` | `{job?}` | Cancels a job (and a separation waiting for it), or every unfinished job. |

`stems.separate` and `stems.install_model` work everywhere commands run, including the headless
CLI and MCP. In the app, prefer `stems.start` so the window keeps drawing.

```sh
soundcraft-cli app --port $P stems.start '{"clips":[3],"stems":["vocals"],"instrumental":true,"download":true}'
soundcraft-cli app --port $P jobs.status '{"job":2}'
SOUNDCRAFT_MODELS_DIR=~/models soundcraft-cli run --cmd 'file.import_audio={"path":"song.mp3"}' --cmd 'stems.separate={"clips":[3]}' --save out.scraft
```

The app keeps models in its settings folder (`Models`). Elsewhere the folder is
`SOUNDCRAFT_MODELS_DIR`, or `SoundCraft Models` in the system temp directory.
