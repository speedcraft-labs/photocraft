use criterion::{Criterion, criterion_group, criterion_main};
use photocraft_psd::{Compression, ImageData, PlaneLayout, PsdFile};
use std::path::{Path, PathBuf};

const MIN_DECODED_BYTES: usize = 1 << 20;
const SAMPLE_COUNT: usize = 5;

struct Sample {
    name: String,
    data: Vec<u8>,
    layout: PlaneLayout,
    decoded_bytes: usize,
}

fn collect_psd_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect_psd_files(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("psd") || ext.eq_ignore_ascii_case("psb")) {
            out.push(path);
        }
    }
}

fn offer_sample(samples: &mut Vec<Sample>, name: String, data: Vec<u8>, layout: PlaneLayout) {
    let Ok(decoded_bytes) = layout.decoded_len() else { return };
    if decoded_bytes < MIN_DECODED_BYTES {
        return;
    }
    if samples.iter().any(|sample| sample.layout == layout && sample.data == data) {
        return;
    }
    samples.push(Sample { name, data, layout, decoded_bytes });
    samples.sort_by(|a, b| b.decoded_bytes.cmp(&a.decoded_bytes).then_with(|| a.name.cmp(&b.name)));
    samples.truncate(SAMPLE_COUNT);
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3))
}

fn real_rle_samples() -> Vec<Sample> {
    let corpus = std::env::var_os("ARTCRAFT_CORPUS").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("corpus"));
    let roots = if corpus.join("psd").is_dir() {
        [corpus.join("psd"), corpus.join("psd-tools")].into_iter().filter(|path| path.is_dir()).collect::<Vec<_>>()
    } else {
        vec![corpus]
    };

    let mut samples = Vec::new();
    for root in roots {
        let mut paths = Vec::new();
        collect_psd_files(&root, &mut paths);
        paths.sort();
        for path in paths {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(file) = PsdFile::from_bytes(&bytes) else { continue };
            let corpus_name = root.file_name().and_then(|name| name.to_str()).unwrap_or("psd");
            let relative = format!("{corpus_name}_{}", path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace(['/', '\\'], "_"));

            if file.image_data.compression == Compression::Rle {
                let layout = ImageData::layout(&file.header);
                if let Ok(data) = file.image_data.encoded() {
                    offer_sample(&mut samples, format!("{relative}_merged"), data.into_owned(), layout);
                }
            }
            if let Some(info) = &file.layer_info {
                for (index, layer) in info.layers.iter().enumerate() {
                    let (width, height) = (layer.rect.width(), layer.rect.height());
                    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else { continue };
                    for channel in &layer.channels {
                        if channel.compression != Some(Compression::Rle) {
                            continue;
                        }
                        let layout = PlaneLayout { planes: 1, width, height, depth: file.header.depth, version: file.header.version };
                        if let Ok(data) = channel.encoded() {
                            offer_sample(&mut samples, format!("{relative}_layer{index}_channel{}", channel.id), data.into_owned(), layout);
                        }
                    }
                }
            }
        }
    }
    samples
}

fn rle_decode(c: &mut Criterion) {
    let samples = real_rle_samples();
    assert!(!samples.is_empty(), "no corpus PackBits input of at least 1 MiB; fetch `cargo xtask corpus --psd`");
    eprintln!("PackBits corpus benchmark inputs:");
    for sample in &samples {
        let decoded = photocraft_psd::compression::decode_planes(Compression::Rle, &sample.data, &sample.layout).unwrap();
        assert_eq!(decoded.len(), sample.decoded_bytes);
        eprintln!("  {} ({} decoded bytes, {} encoded bytes, FNV-1a {:016x})", sample.name, sample.decoded_bytes, sample.data.len(), digest(&decoded));
    }

    let mut group = c.benchmark_group("psd_rle_corpus");
    group.sample_size(10);
    for sample in &samples {
        group.bench_function(&sample.name, |b| {
            b.iter(|| photocraft_psd::compression::decode_planes(Compression::Rle, std::hint::black_box(&sample.data), &sample.layout).unwrap())
        });
    }
    group.finish();
}

criterion_group!(benches, rle_decode);
criterion_main!(benches);
