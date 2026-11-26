use std::{collections::BTreeMap, time::Duration};

use serde::Serialize;

use super::{Error, Options, executable::capture};

#[derive(Clone, Default, Serialize)]
pub struct Inventory {
    pub ffmpeg: Version,
    pub filter: Vec<Item>,
    pub hwaccels: Vec<Item>,
    pub codecs: Codecs,
    pub devices: Formats<Device>,
    pub formats: Formats<Item>,
    pub protocols: Protocols,
    pub ffprobe_available: bool,
    pub ffprobe_version: Option<String>,
}

impl Inventory {
    pub(crate) fn has_video_encoder(&self, name: &str) -> bool {
        self.codecs
            .video
            .iter()
            .any(|codec| codec.encoders.iter().any(|encoder| encoder == name))
    }
}

#[derive(Clone, Default, Serialize)]
pub struct Version {
    pub version: String,
    pub compiler: String,
    pub configuration: String,
    libraries: Vec<Library>,
}

#[derive(Clone, Serialize)]
struct Library {
    name: String,
    compiled: String,
    linked: String,
}

#[derive(Clone, Default, Serialize)]
pub struct Item {
    id: String,
    name: String,
}

#[derive(Clone, Default, Serialize)]
pub struct Device {
    id: String,
    name: String,
    devices: Vec<Item>,
}

#[derive(Clone, Default, Serialize)]
pub struct Formats<T> {
    demuxers: Vec<T>,
    muxers: Vec<T>,
}

#[derive(Clone, Default, Serialize)]
pub struct Protocols {
    input: Vec<Item>,
    output: Vec<Item>,
}

#[derive(Clone, Default, Serialize)]
pub struct Codecs {
    audio: Vec<Codec>,
    video: Vec<Codec>,
    subtitle: Vec<Codec>,
}

#[derive(Clone, Serialize)]
struct Codec {
    id: String,
    name: String,
    encoders: Vec<String>,
    decoders: Vec<String>,
}

pub async fn inspect(options: &Options) -> Result<Inventory, Error> {
    let inspect = |flag: &'static str| async move {
        let result = capture(
            &options.ffmpeg,
            &["-hide_banner".into(), flag.into()],
            Duration::from_secs(options.timeout),
            options.launcher.as_deref(),
        )
        .await?;
        if !result.success {
            return Err(Error::Unavailable("FFmpeg capability discovery failed"));
        }
        Ok::<_, Error>(if result.stdout.is_empty() {
            result.stderr
        } else {
            result.stdout
        })
    };
    let version = inspect("-version").await?;
    let (codecs, encoders, decoders, formats, filters, protocols, hwaccels, devices) = tokio::try_join!(
        inspect("-codecs"),
        inspect("-encoders"),
        inspect("-decoders"),
        inspect("-formats"),
        inspect("-filters"),
        inspect("-protocols"),
        inspect("-hwaccels"),
        inspect("-devices"),
    )?;
    let probe = capture(
        &options.ffprobe,
        &["-version".into()],
        Duration::from_secs(options.timeout),
        options.launcher.as_deref(),
    )
    .await;
    let mut inventory = Inventory::default();
    if let Ok(probe) = probe
        && probe.success
    {
        inventory.ffprobe_available = true;
        inventory.ffprobe_version = probe
            .stdout
            .lines()
            .chain(probe.stderr.lines())
            .find_map(|line| line.strip_prefix("ffprobe version ").map(version_number))
            .filter(|version| !version.is_empty());
    }
    for line in version.lines() {
        if let Some(value) = line.strip_prefix("ffmpeg version ") {
            inventory.ffmpeg.version = version_number(value);
        } else if let Some(value) = line.trim().strip_prefix("built with ") {
            inventory.ffmpeg.compiler = value.to_owned();
        } else if let Some(value) = line.trim().strip_prefix("configuration: ") {
            inventory.ffmpeg.configuration = value.to_owned();
        } else if line.starts_with("lib")
            && let Some((name, versions)) = line.split_once(char::is_whitespace)
            && let Some((compiled, linked)) = versions.split_once('/')
        {
            inventory.ffmpeg.libraries.push(Library {
                name: name.into(),
                compiled: compiled.trim().into(),
                linked: linked.trim().into(),
            });
        }
    }
    if inventory.ffmpeg.version.is_empty() {
        return Err(Error::Unavailable("unrecognized FFmpeg version output"));
    }
    let encoders = codec_implementations(&encoders);
    let decoders = codec_implementations(&decoders);
    for (flags, id, description) in rows(&codecs) {
        if flags.len() != 6 {
            continue;
        }
        let target = match flags.as_bytes()[2] {
            b'A' => &mut inventory.codecs.audio,
            b'V' => &mut inventory.codecs.video,
            b'S' => &mut inventory.codecs.subtitle,
            _ => continue,
        };
        let names = |direction: &str, known: &BTreeMap<&str, Vec<&str>>| {
            let mut found: Vec<_> = known.get(id).into_iter().flatten().copied().collect();
            if let Some(value) = description
                .split_once(direction)
                .and_then(|(_, tail)| tail.split_once(')'))
            {
                found.extend(value.0.split_whitespace());
            }
            found.sort_unstable();
            found.dedup();
            found.into_iter().map(str::to_owned).collect()
        };
        target.push(Codec {
            id: id.into(),
            name: description.split(" (").next().unwrap_or(description).into(),
            encoders: names("(encoders: ", &encoders),
            decoders: names("(decoders: ", &decoders),
        });
    }
    for (flags, ids, name) in rows(&formats) {
        if flags.bytes().any(|c| !matches!(c, b'D' | b'E' | b'd')) {
            continue;
        }
        for id in ids.split(',') {
            let item = Item {
                id: id.into(),
                name: name.into(),
            };
            if flags.contains('D') {
                inventory.formats.demuxers.push(item.clone());
            }
            if flags.contains('E') {
                inventory.formats.muxers.push(item);
            }
        }
    }
    for (flags, ids, name) in rows(&devices) {
        if flags.bytes().any(|c| !matches!(c, b'D' | b'E')) {
            continue;
        }
        for id in ids.split(',') {
            let device = Device {
                id: id.into(),
                name: name.into(),
                devices: Vec::new(),
            };
            if flags.contains('D') {
                inventory.devices.demuxers.push(device.clone());
            }
            if flags.contains('E') {
                inventory.devices.muxers.push(device);
            }
        }
    }
    inventory.filter = rows(&filters)
        .filter(|(flags, _, _)| {
            flags.len() == 3
                && flags
                    .bytes()
                    .all(|c| matches!(c, b'.' | b'T' | b'S' | b'C'))
        })
        .filter_map(|(_, id, rest)| {
            let (_, name) = rest.split_once(char::is_whitespace)?;
            Some(Item {
                id: id.into(),
                name: name.trim().into(),
            })
        })
        .collect();
    inventory.hwaccels = hwaccels
        .lines()
        .skip_while(|line| !line.starts_with("Hardware"))
        .skip(1)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| Item {
            id: id.into(),
            name: id.into(),
        })
        .collect();
    let mut input = true;
    for token in protocols.lines().map(str::trim) {
        match token {
            "Input:" => input = true,
            "Output:" => input = false,
            s if !s.is_empty() && !s.contains(char::is_whitespace) => {
                let items = if input {
                    &mut inventory.protocols.input
                } else {
                    &mut inventory.protocols.output
                };
                items.push(Item {
                    id: s.into(),
                    name: s.into(),
                });
            }
            _ => {}
        }
    }
    Ok(inventory)
}

fn rows(data: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    data.lines().filter_map(|line| {
        let (flags, rest) = line.trim().split_once(char::is_whitespace)?;
        let (id, name) = rest.trim_start().split_once(char::is_whitespace)?;
        (!id.starts_with('=')).then_some((flags, id, name.trim()))
    })
}

fn codec_implementations(data: &str) -> BTreeMap<&str, Vec<&str>> {
    let mut result = BTreeMap::<_, Vec<_>>::new();
    for (flags, id, description) in rows(data) {
        if flags.len() != 6 {
            continue;
        }
        let codec = description
            .rsplit_once("(codec ")
            .and_then(|(_, tail)| tail.strip_suffix(')'))
            .unwrap_or(id);
        result.entry(codec).or_default().push(id);
    }
    result
}

fn version_number(line: &str) -> String {
    let token = line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('n');
    let version: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    match version.split('.').count() {
        1 if !version.is_empty() => format!("{version}.0.0"),
        2 => format!("{version}.0"),
        _ => version,
    }
}
