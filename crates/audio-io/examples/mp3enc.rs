//! Encode a WAV to MP3 with the built-in encoder: `mp3enc in.wav out.mp3 [kbps]`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else { return Err("usage: mp3enc in.wav out.mp3 [kbps]".into()) };
    let kbps = args.get(3).and_then(|k| k.parse().ok());
    let bytes = std::fs::read(input)?;
    let (_, buf) = soundcraft_audio_io::decode(&bytes, Some("wav"))?;
    let t = std::time::Instant::now();
    let opts = soundcraft_audio_io::EncodeOptions { format: soundcraft_audio_io::FileFormat::Mp3, bitrate_kbps: kbps, ..Default::default() };
    let out = soundcraft_audio_io::encode(&buf, &opts)?;
    eprintln!("encoded {:.1} s of audio in {:.2} s", buf.channels[0].len() as f64 / f64::from(buf.sample_rate), t.elapsed().as_secs_f64());
    std::fs::write(output, out)?;
    Ok(())
}
