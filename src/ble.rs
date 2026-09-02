use anyhow::anyhow;
use once_cell::sync::Lazy;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use serde::{Deserialize, Deserializer};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

static MGR: Lazy<PacketManager> = Lazy::new(PacketManager::new);

#[derive(Clone, PartialEq, Eq)]
pub struct HexBytes(Vec<u8>);

impl std::fmt::Debug for HexBytes {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.write_fmt(format_args!("{:02X?}", self.0))
    }
}

#[allow(clippy::type_complexity)]
pub struct PacketCodec {
    encode: Box<dyn Fn(&dyn Any) -> anyhow::Result<Vec<u8>> + Sync + Send>,
    decode: Box<dyn Fn(&[u8]) -> anyhow::Result<GoveeBlePacket> + Sync + Send>,
    supported_skus: &'static [&'static str],
    type_id: TypeId,
}

impl PacketCodec {
    pub fn new<T: 'static>(
        supported_skus: &'static [&'static str],
        encode: impl Fn(&T) -> anyhow::Result<Vec<u8>> + 'static + Sync + Send,
        decode: impl Fn(&[u8]) -> anyhow::Result<GoveeBlePacket> + 'static + Sync + Send,
    ) -> Self {
        Self {
            encode: Box::new(move |any| {
                let type_id = TypeId::of::<T>();
                let value = any.downcast_ref::<T>().ok_or_else(|| {
                    anyhow!("cannot downcast to {type_id:?} in PacketCodec encoder")
                })?;
                (encode)(value)
            }),
            decode: Box::new(decode),
            supported_skus,
            type_id: TypeId::of::<T>(),
        }
    }
}

pub struct PacketManager {
    codec_by_sku: Mutex<HashMap<String, HashMap<TypeId, Arc<PacketCodec>>>>,
    all_codecs: Vec<Arc<PacketCodec>>,
}

impl PacketManager {
    fn map_for_sku(&self, sku: &str) -> MappedMutexGuard<'_, HashMap<TypeId, Arc<PacketCodec>>> {
        MutexGuard::map(self.codec_by_sku.lock(), |codecs| {
            codecs.entry(sku.to_string()).or_insert_with(|| {
                let mut map = HashMap::new();

                for codec in &self.all_codecs {
                    if codec.supported_skus.contains(&sku)
                        && map.insert(codec.type_id, codec.clone()).is_some()
                    {
                        eprintln!("Conflicting PacketCodecs for {sku} {:?}", codec.type_id);
                    }
                }

                map
            })
        })
    }

    fn resolve_by_sku(&self, sku: &str, type_id: &TypeId) -> anyhow::Result<Arc<PacketCodec>> {
        let map = self.map_for_sku(sku);

        map.get(type_id)
            .cloned()
            .ok_or_else(|| anyhow!("sku {sku} has no codec for type {type_id:?}"))
    }

    pub fn decode_for_sku(&self, sku: &str, data: &[u8]) -> GoveeBlePacket {
        let map = self.map_for_sku(sku);

        for codec in map.values() {
            if let Ok(value) = (codec.decode)(data) {
                return value;
            }
        }

        GoveeBlePacket::Generic(HexBytes(data.to_vec()))
    }

    pub fn encode_for_sku<T: 'static>(&self, sku: &str, value: &T) -> anyhow::Result<Vec<u8>> {
        let type_id = TypeId::of::<T>();
        let codec = self.resolve_by_sku(sku, &type_id)?;

        (codec.encode)(value)
    }

    pub fn new() -> Self {
        let mut all_codecs = vec![];

        macro_rules! encode_body {
            // Tail case: nothing to do
            ($target:expr,$input:expr,) => {};

            // Match a constant byte; emit it
            ($target:expr,$input:expr, $expected:literal, $($tail:tt)*) => {
                    $target.push($expected);
                    encode_body!($target, $input, $($tail)*);
            };

            // Match a field; emit it from the struct
            ($target:expr, $input:expr, $field_name:ident, $($tail:tt)*) => {
                    $input.$field_name.encode_param($target);
                    encode_body!($target, $input, $($tail)*);
            };
        }

        macro_rules! decode_body {
            // Tail case; verify that remaining bytes are zero
            ($target:expr, $data:expr,) => {
                while !$data.is_empty() {
                    anyhow::ensure!($data[0] == 0);
                    $data = &$data[1..];
                }
            };

            // Match a constant byte; check that it is what we expect
            ($target:expr, $data:expr, $expected:literal, $($tail:tt)*) => {
                    let maybe_byte = $data.get(0);
                    anyhow::ensure!(maybe_byte == Some(&$expected),"expected {} but got {maybe_byte:?}", $expected);
                    $data = &$data[1..];
                    decode_body!($target, $data, $($tail)*);
            };

            // Match a field; parse it into the struct
            ($target:expr, $data:expr, $field_name:ident, $($tail:tt)*) => {
                    let remain = $target.$field_name.decode_param($data)?;
                    $data = remain;
                    decode_body!($target, $data, $($tail)*);
            };
        }

        /// Helper for defining a PacketCodec.
        /// The first param is the list of SKUs which are known to support
        /// this packet.
        /// The second parameter is the name of the type which will be
        /// encoded into raw bytes when encoding. It must impl Default.
        /// The third parameter is the name of the GoveeBlePacket enum
        /// variant that holds that type.
        /// The subsequent parameters are rules that match the bytes
        /// in the packet when decoding, or form the bytes in the packet
        /// when encoding. They are listed in the same sequence that they
        /// have in the packet.
        macro_rules! packet {
            ($skus:expr, $struct:ident, $variant:ident, $($body:tt)*) => {
                PacketCodec::new(
                    $skus,
                    |input_value: &$struct| {
                        let mut bytes = vec![];
                        encode_body!(&mut bytes, input_value, $($body)*);
                        Ok(finish(bytes))
                    },
                    |data| {
                        let mut data = &data[0..data.len().saturating_sub(1)];
                        let mut value = $struct::default();
                        decode_body!(&mut value, data, $($body)*);
                        Ok(GoveeBlePacket::$variant(value))
                    }
                )
            }
        }

        all_codecs.push(packet!(
            &["H7160"],
            SetHumidifierMode,
            SetHumidifierMode,
            0x33,
            0x05,
            mode,
            param,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            NotifyHumidifierMode,
            NotifyHumidifierMode,
            0xaa,
            0x05,
            0x00,
            mode,
            param,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            HumidifierAutoMode,
            NotifyHumidifierAutoMode,
            0xaa,
            0x05,
            0x03,
            target_humidity,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            NotifyHumidifierNightlightParams,
            NotifyHumidifierNightlight,
            0xaa,
            0x1b,
            on,
            brightness,
            r,
            g,
            b,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            SetHumidifierNightlightParams,
            SetHumidifierNightlight,
            0x33,
            0x1b,
            on,
            brightness,
            r,
            g,
            b,
        ));
        // H7180 rice cooker report frames. Decode-only: the write
        // direction has never been captured, so no encoder exists and
        // none is fabricated. See the type docs for the evidence trail.
        all_codecs.push(PacketCodec::new(
            &["H7180"],
            NotifyRiceCookerActiveProgram::encode,
            NotifyRiceCookerActiveProgram::decode,
        ));
        all_codecs.push(PacketCodec::new(
            &["H7180"],
            NotifyRiceCookerProgramPhase::encode,
            NotifyRiceCookerProgramPhase::decode,
        ));
        all_codecs.push(PacketCodec::new(
            &["H7180"],
            NotifyRiceCookerProgramParams::encode,
            NotifyRiceCookerProgramParams::decode,
        ));

        all_codecs.push(PacketCodec::new(
            &["Generic:Light"],
            SetSceneCode::encode,
            SetSceneCode::decode,
        ));

        all_codecs.push(PacketCodec::new(
            &["Generic:Light"],
            SetMusicPalette::encode,
            SetMusicPalette::decode,
        ));

        all_codecs.push(packet!(
            &["Generic:Light"],
            SetDevicePower,
            SetDevicePower,
            0x33,
            0x01,
            on,
        ));

        Self {
            codec_by_sku: Mutex::new(HashMap::new()),
            all_codecs: all_codecs.into_iter().map(Arc::new).collect(),
        }
    }
}

pub trait DecodePacketParam {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]>;
    fn encode_param(&self, target: &mut Vec<u8>);
}

impl DecodePacketParam for u8 {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        *self = *data.first().ok_or_else(|| anyhow!("EOF"))?;
        Ok(&data[1..])
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(*self);
    }
}

impl DecodePacketParam for u16 {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        let lo = *data.first().ok_or_else(|| anyhow!("EOF"))?;
        let hi = *data.get(1).ok_or_else(|| anyhow!("EOF"))?;
        *self = ((hi as u16) << 8) | lo as u16;
        Ok(&data[2..])
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        let hi = (*self >> 8) as u8;
        let lo = (*self & 0xff) as u8;
        target.push(lo);
        target.push(hi);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SetHumidifierNightlightParams {
    pub on: bool,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub brightness: u8,
}

impl From<NotifyHumidifierNightlightParams> for SetHumidifierNightlightParams {
    fn from(val: NotifyHumidifierNightlightParams) -> Self {
        SetHumidifierNightlightParams {
            on: val.on,
            r: val.r,
            g: val.g,
            b: val.b,
            brightness: val.brightness,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NotifyHumidifierNightlightParams {
    pub on: bool,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub brightness: u8,
}

/// Data is offset by 128 with increments of 1%,
/// so 0% is 128, 100% is 228%
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetHumidity(u8);

impl From<TargetHumidity> for u8 {
    fn from(val: TargetHumidity) -> Self {
        val.0
    }
}

impl DecodePacketParam for TargetHumidity {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        self.0.decode_param(data)
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(self.0);
    }
}

impl TargetHumidity {
    pub fn as_percent(&self) -> u8 {
        self.0 & 0x7f
    }

    pub fn into_inner(self) -> u8 {
        self.0
    }

    pub fn from_percent(percent: u8) -> Self {
        Self(percent + 128)
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetHumidifierMode {
    pub mode: u8,
    pub param: u8,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct NotifyHumidifierMode {
    pub mode: u8,
    pub param: u8,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct HumidifierAutoMode {
    pub target_humidity: TargetHumidity,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetSceneCode {
    code: u16,
    scence_param: String,
}

impl SetSceneCode {
    pub fn new(code: u16, scence_param: String) -> Self {
        Self { code, scence_param }
    }

    /// For reference, see:
    /// <https://github.com/egold555/Govee-Reverse-Engineering/issues/11#issuecomment-2565692233>
    /// <https://github.com/AlgoClaw/Govee/blob/main/decoded/explanation>
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let bytes = data_encoding::BASE64.decode(self.scence_param.as_bytes())?;

        let mut data = vec![0xa3, 0x00, 0x01, 0x00 /* line count */, 0x02];
        let mut num_lines = 0u8;
        let mut last_line_marker = 1;

        for b in bytes {
            if data.len().is_multiple_of(19) {
                num_lines += 1;

                data.push(0xa3);
                last_line_marker = data.len();

                data.push(num_lines);
            }

            data.push(b);
        }
        // The last line uses 0xff as the indicator, rather than its line number
        data[last_line_marker] = 0xff;
        // back-patch the number of lines into the packet
        data[3] = num_lines + 1;

        // Now apply padding and checksums
        let mut padded = vec![];
        for chunk in data.chunks(19) {
            let mut padded_chunk = chunk.to_vec();
            padded_chunk = finish(padded_chunk);
            padded.append(&mut padded_chunk);
        }

        // and finally encode the scene code as the final packet "line"
        let hi = (self.code >> 8) as u8;
        let lo = (self.code & 0xff) as u8;
        padded.append(&mut finish(vec![0x33, 0x05, 0x04, lo, hi]));
        Ok(padded)
    }

    fn decode(_data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        anyhow::bail!("SetSceneCode::decode is not implemented");
    }
}

/// Music mode with a caller-chosen colour palette, over the device's
/// internal protocol. A sibling of [`SetSceneCode`]: same `a3`
/// fragmentation, and the activation opcode is in the same `33 05 XX`
/// "set mode, subtype" family (subtype `0x13` = music, `0x04` = scene).
///
/// Frame sequence (each frame padded to 19 bytes + XOR checksum):
///
/// ```text
/// a3 00 01 02 41 <profile> <ncolors> <first 12 palette bytes>
/// a3 ff <remaining palette bytes>
/// 33 05 13 <profile> <sensitivity>
/// ```
///
/// The palette body is `ncolors` RGB triples; with at most 7 colours the
/// classic dialect always fits in exactly two `a3` fragments. `profile`
/// is the SKU-specific style id from [`crate::music::music_profile`] —
/// it is not the Platform API's `musicMode` enum value.
///
/// Frame layout captured from app traffic. Hardware verification: 2- and
/// 5-colour writes (the latter exercising a non-empty `a3 ff` spill) were
/// accepted by an H607C, confirmed via its IoT ptReal echo and `aa 05 13`
/// read-back (`proof/music-palette-runtime.log`,
/// `proof/music-palette-spill-runtime.log`). Other palette sizes are
/// golden-tested against the Python reference only. Methodology and
/// per-frame semantics in `docs/MUSIC_MODE.md`.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetMusicPalette {
    pub profile: u8,
    pub colors: Vec<[u8; 3]>,
    pub sensitivity: u8,
}

impl SetMusicPalette {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            (1..=7).contains(&self.colors.len()),
            "music palette wants 1..=7 colours, got {}",
            self.colors.len()
        );
        anyhow::ensure!(
            self.sensitivity <= 100,
            "sensitivity is a percentage, got {}",
            self.sensitivity
        );

        let flat: Vec<u8> = self.colors.iter().flatten().copied().collect();
        let (head, rest) = flat.split_at(flat.len().min(12));

        let mut first = vec![
            0xa3,
            0x00,
            0x01,
            0x02,
            0x41,
            self.profile,
            self.colors.len() as u8,
        ];
        first.extend_from_slice(head);

        let mut second = vec![0xa3, 0xff];
        second.extend_from_slice(rest);

        let mut frames = finish(first);
        frames.append(&mut finish(second));
        frames.append(&mut finish(vec![
            0x33,
            0x05,
            0x13,
            self.profile,
            self.sensitivity,
        ]));
        Ok(frames)
    }

    fn decode(_data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        anyhow::bail!("SetMusicPalette::decode is not implemented");
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetDevicePower {
    pub on: bool,
}

/// Validate a 20-byte `0xaa`-prefixed rice cooker report frame and
/// return its body (everything between the `0xaa` marker and the
/// trailing checksum). The final byte is an XOR of the preceding 19;
/// that held for every one of the 201 `0xaa` frames in the 2026-09-01
/// H7180/H717A/H7102 account-topic capture, so a mismatch here means
/// the frame is not something we understand and it falls through to
/// [`GoveeBlePacket::Generic`].
fn rice_cooker_report_body(data: &[u8]) -> anyhow::Result<&[u8]> {
    anyhow::ensure!(data.len() == 20, "rice cooker reports are 20 bytes");
    anyhow::ensure!(data[0] == 0xaa, "not an 0xaa report frame");
    anyhow::ensure!(
        calculate_checksum(&data[..19]) == data[19],
        "xor checksum mismatch"
    );
    Ok(&data[1..19])
}

/// `AA 05 00 <program>` from the H7180 rice cooker: the currently
/// active program slot, `0` when the cooker is in standby. Observed
/// live on 2026-09-01 (wez/govee2mqtt#173): the value moved
/// `0 -> 1 -> 0 -> 4 -> 0 -> 3 -> 0` in lockstep with programs being
/// started and stopped from the Govee app, and every `AA 19` frame in
/// the same bursts mirrored the same id. The same `aa 05 00` family
/// carries the active mode/slot on the H7160 humidifier and the
/// H7171/H717A kettles (homebridge-govee `kettle.js` function `0500`).
/// Which physical cooking function each id maps to is not yet known,
/// so no names are assigned anywhere.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifyRiceCookerActiveProgram {
    pub program: u8,
}

impl NotifyRiceCookerActiveProgram {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("NotifyRiceCookerActiveProgram is a device report; no encoder");
    }

    fn decode(data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        let body = rice_cooker_report_body(data)?;
        anyhow::ensure!(
            body[0] == 0x05 && body[1] == 0x00,
            "not an active-program frame"
        );
        anyhow::ensure!(
            body[3..].iter().all(|&b| b == 0),
            "unexpected trailing data"
        );
        Ok(GoveeBlePacket::NotifyRiceCookerActiveProgram(Self {
            program: body[2],
        }))
    }
}

/// `AA 19 <program> <phase>` from the H7180 rice cooker. In the
/// 2026-09-01 capture `<program>` always matched the id in the
/// `AA 05 00` frame of the same burst (including `3`, which does not
/// exist in the kettle dialect, where `aa 19` byte 2 is a heating
/// state instead — the cooker evidently repurposed the family).
/// `<phase>` was `2` while program 1 ran and `1` for programs 3/4;
/// its meaning is unconfirmed, so it is surfaced only as a raw
/// diagnostic attribute.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifyRiceCookerProgramPhase {
    pub program: u8,
    pub phase: u8,
}

impl NotifyRiceCookerProgramPhase {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("NotifyRiceCookerProgramPhase is a device report; no encoder");
    }

    fn decode(data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        let body = rice_cooker_report_body(data)?;
        anyhow::ensure!(body[0] == 0x19, "not a program-phase frame");
        anyhow::ensure!(
            body[3..].iter().all(|&b| b == 0),
            "unexpected trailing data"
        );
        Ok(GoveeBlePacket::NotifyRiceCookerProgramPhase(Self {
            program: body[1],
            phase: body[2],
        }))
    }
}

/// `AA 05 <program != 0> <params...>` from the H7180 rice cooker: the
/// parameter block of a program. The layout is program-specific, but
/// every program observed on 2026-09-01 carried the block
/// `01 <temp:u16be> 02 D0` at a fixed per-program offset, where
/// `<temp>` is hundredths of a degree Fahrenheit — the same scaling
/// homebridge-govee documents for the kettle family ("two bytes of
/// hundredths of a degree fahrenheit") and the same capture's H717A
/// `aa 10 01 1C 84` = 73.00°F corroborates. The captured cooker values
/// were 13100/14900/11300 centi-°F = 131/149/113°F = exactly 55/65/45°C,
/// moving only when a temperature was changed in the Govee app, so this
/// is read as a set-point, not a probe reading. `0x02D0` (720, presumed
/// minutes = 12h) never varied and is not decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyRiceCookerProgramParams {
    pub program: u8,
    /// Raw parameter bytes (frame bytes 3..=18), preserved so the
    /// undecoded remainder stays visible for protocol work.
    pub params: [u8; 16],
}

impl NotifyRiceCookerProgramParams {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("NotifyRiceCookerProgramParams is a device report; no encoder");
    }

    fn decode(data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        let body = rice_cooker_report_body(data)?;
        anyhow::ensure!(body[0] == 0x05, "not a program frame");
        anyhow::ensure!(body[1] != 0x00, "program 0 is the active-program frame");
        let mut params = [0u8; 16];
        params.copy_from_slice(&body[2..]);
        Ok(GoveeBlePacket::NotifyRiceCookerProgramParams(Self {
            program: body[1],
            params,
        }))
    }

    /// The program's set temperature in hundredths of a degree
    /// Fahrenheit, for the program layouts confirmed by the 2026-09-01
    /// capture. Unknown program ids return `None` rather than guessing
    /// an offset. The plausibility window is the one homebridge-govee
    /// applies to kettle temperatures (32.00°F..=230.00°F).
    pub fn set_temperature_centi_fahrenheit(&self) -> Option<u16> {
        let idx = self.temperature_index()?;
        let value = u16::from_be_bytes([self.params[idx], self.params[idx + 1]]);
        (3200..=23000).contains(&value).then_some(value)
    }

    /// Byte offset of this program's set-temperature field. Established
    /// by the 2026-09-01 capture and re-confirmed independently on
    /// 2026-09-02, when the temperature was raised mid-run on the
    /// appliance and only this field moved (13100 -> 14900, i.e.
    /// 131 -> 149 degF). Unknown program ids return `None` rather than
    /// guessing an offset. Every other field below is located relative
    /// to this one, so they inherit the same per-program safety.
    fn temperature_index(&self) -> Option<usize> {
        match self.program {
            1 => Some(4),
            3 => Some(5),
            4 => Some(6),
            _ => None,
        }
    }

    /// The program's configured duration in minutes, held immediately
    /// after the temperature field.
    ///
    /// Confirmed 2026-09-02: all three known program layouts carried
    /// 720 at this same relative offset, and the value halved to 360
    /// the instant the keep-warm duration was changed from 12h to 6h
    /// on the appliance -- which also fixes the unit as minutes.
    pub fn duration_minutes(&self) -> Option<u16> {
        let idx = self.temperature_index()? + 2;
        let value = u16::from_be_bytes([self.params[idx], self.params[idx + 1]]);
        (1..=1440).contains(&value).then_some(value)
    }

    /// Minutes until a delayed program is scheduled to FINISH, or
    /// `None` when no delayed start is set. Held three bytes before
    /// the temperature field.
    ///
    /// Confirmed 2026-09-02 against two independent schedules set
    /// minutes apart: rice read 173 for a 01:00 finish and steam read
    /// 473 for a 06:00 finish, both from 22:07 local -- each landing
    /// exactly on the requested wall-clock time.
    ///
    /// Note this is deliberately NOT sourced from the `AA 16 .. FF FF
    /// FF FF` frame. That frame looks like a timer and was originally
    /// guessed to be one, but it stayed pinned at its unset sentinel
    /// through both schedules above, so it is something else.
    pub fn scheduled_finish_in_minutes(&self) -> Option<u16> {
        let idx = self.temperature_index()?.checked_sub(3)?;
        let value = u16::from_be_bytes([self.params[idx], self.params[idx + 1]]);
        (1..=1440).contains(&value).then_some(value)
    }
}

/// Build the "set active program" command for an H7180.
///
/// Reports arrive as `AA 05 00 <program>`; Govee's protocol uses `0x33` as
/// the WRITE prefix and `0xAA` as the REPORT prefix throughout this file
/// (see `SetSceneCode`, which encodes `33 05 04 ..`), so the write mirror
/// of that report is `33 05 00 <program>`. `finish` applies the same
/// 20-byte padding and XOR checksum every other command uses.
///
/// Program 0 is the stop/cancel case: the appliance reports `program: 0`
/// when a run ends naturally, so sending 0 asks it to return to standby.
pub fn rice_cooker_set_program_command(program: u8) -> Base64HexBytes {
    Base64HexBytes(HexBytes(finish(vec![0x33, 0x05, 0x00, program])))
}

/// Reverse of [`rice_cooker_program_name`], for turning a Home Assistant
/// select option back into a program id. Returns `None` for anything not
/// in the confirmed map so an unexpected option can never be sent to a
/// heating appliance as a guessed opcode.
pub fn rice_cooker_program_id(name: &str) -> Option<u8> {
    match name {
        "Standby" => Some(0),
        "Rice" => Some(1),
        "Saute" => Some(2),
        "Steam" => Some(3),
        "Slow Cook" => Some(4),
        "DIY" => Some(5),
        _ => None,
    }
}

/// The options offered by the Home Assistant select, in program-id order.
pub fn rice_cooker_program_options() -> Vec<String> {
    (0..=5u8).map(rice_cooker_program_name).collect()
}

/// Human-readable name for an H7180 program id.
///
/// Every mapping here was confirmed on 2026-09-02 by Colin naming each
/// program aloud as he started it on the appliance, so these are
/// observations rather than inferences. Unknown ids fall back to the
/// numeric form rather than inventing a name.
///
/// Keep Warm is deliberately absent: it is not a program. It appears as
/// program 0 with phase 8, which is why phase 8 also shows up in the
/// tail of a finished Steam run.
pub fn rice_cooker_program_name(program: u8) -> String {
    match program {
        0 => "Standby".to_string(),
        1 => "Rice".to_string(),
        2 => "Saute".to_string(),
        3 => "Steam".to_string(),
        4 => "Slow Cook".to_string(),
        5 => "DIY".to_string(),
        n => format!("Program {n}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoveeBlePacket {
    Generic(HexBytes),
    #[allow(unused)] // can remove if/when SetSceneCode::decode has an impl
    SetSceneCode(SetSceneCode),
    SetDevicePower(SetDevicePower),
    SetHumidifierNightlight(SetHumidifierNightlightParams),
    NotifyHumidifierMode(NotifyHumidifierMode),
    SetHumidifierMode(SetHumidifierMode),
    NotifyHumidifierAutoMode(HumidifierAutoMode),
    NotifyHumidifierNightlight(NotifyHumidifierNightlightParams),
    NotifyRiceCookerActiveProgram(NotifyRiceCookerActiveProgram),
    NotifyRiceCookerProgramPhase(NotifyRiceCookerProgramPhase),
    NotifyRiceCookerProgramParams(NotifyRiceCookerProgramParams),
}

#[derive(Debug)]
pub struct Base64HexBytes(HexBytes);

impl Base64HexBytes {
    pub fn decode_for_sku(&self, sku: &str) -> GoveeBlePacket {
        MGR.decode_for_sku(sku, &self.0 .0)
    }

    pub fn encode_for_sku<T: 'static>(sku: &str, value: &T) -> anyhow::Result<Self> {
        MGR.encode_for_sku(sku, value)
            .map(|bytes| Base64HexBytes(HexBytes(bytes)))
    }

    pub fn base64(&self) -> Vec<String> {
        let mut result = vec![];
        for chunk in self.0 .0.chunks(20) {
            result.push(data_encoding::BASE64.encode(chunk));
        }
        result
    }

    pub fn with_bytes(bytes: Vec<u8>) -> Self {
        Self(HexBytes(finish(bytes)))
    }
}

impl<'de> Deserialize<'de> for Base64HexBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;
        let encoded = String::deserialize(deserializer)?;
        let decoded = data_encoding::BASE64
            .decode(encoded.as_ref())
            .map_err(|e| D::Error::custom(format!("{e:#}")))?;
        Ok(Self(HexBytes(decoded)))
    }
}

fn calculate_checksum(data: &[u8]) -> u8 {
    let mut checksum: u8 = 0;
    for &b in data {
        checksum ^= b;
    }
    checksum
}

fn finish(mut data: Vec<u8>) -> Vec<u8> {
    let checksum = calculate_checksum(&data);
    data.resize(19, 0);
    data.push(checksum);
    data
}

impl DecodePacketParam for bool {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        let mut byte = 0u8;
        let remain = byte.decode_param(data)?;
        *self = itob(&byte);
        Ok(remain)
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(btoi(*self));
    }
}

fn btoi(on: bool) -> u8 {
    if on {
        1
    } else {
        0
    }
}

fn itob(i: &u8) -> bool {
    *i != 0
}

impl GoveeBlePacket {}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn packet_manager() {
        assert_eq!(
            MGR.decode_for_sku(
                "H7160",
                &[0x33, 0x05, 0x01, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23]
            ),
            GoveeBlePacket::SetHumidifierMode(SetHumidifierMode {
                mode: 1,
                param: 0x20
            })
        );

        assert_eq!(
            MGR.encode_for_sku(
                "H7160",
                &SetHumidifierMode {
                    mode: 1,
                    param: 0x20
                }
            )
            .unwrap(),
            vec![0x33, 0x05, 0x01, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23]
        );
    }

    fn round_trip<T: 'static + std::fmt::Debug>(sku: &str, value: &T, expect: GoveeBlePacket) {
        let bytes = Base64HexBytes::encode_for_sku(sku, value).unwrap();
        let decoded = bytes.decode_for_sku(sku);
        assert_eq!(decoded, expect);
    }

    #[test]
    fn basic_round_trip() {
        round_trip(
            "Generic:Light",
            &SetDevicePower { on: true },
            GoveeBlePacket::SetDevicePower(SetDevicePower { on: true }),
        );
        round_trip(
            "H7160",
            &SetHumidifierNightlightParams {
                on: true,
                r: 255,
                g: 69,
                b: 42,
                brightness: 100,
            },
            GoveeBlePacket::SetHumidifierNightlight(SetHumidifierNightlightParams {
                on: true,
                r: 255,
                g: 69,
                b: 42,
                brightness: 100,
            }),
        );
    }

    #[test]
    fn scene_command() {
        const FOREST_SCENCE_PARAM: &str = "AyYAAQAKAgH/GQG0CgoCyBQF//8AAP//////AP//lP8AFAGWAAAAACMAAg8FAgH/FAH7AAAB+goEBP8AtP8AR///4/8AAAAAAAAAABoAAAABAgH/BQHIFBQC7hQBAP8AAAAAAAAAAA==";
        const FOREST_SCENE_CODE: u16 = 212;

        let command = SetSceneCode::new(FOREST_SCENE_CODE, FOREST_SCENCE_PARAM.to_string());

        let padded = command.encode().unwrap();

        println!("data is:");
        let mut hex = String::new();
        for (idx, b) in padded.iter().enumerate() {
            if idx % 20 == 0 && !hex.is_empty() {
                hex.push('\n');
            } else if !hex.is_empty() {
                hex.push(' ');
            }
            hex.push_str(&format!("{b:02x}"));
        }
        println!("{hex}");

        k9::snapshot!(
            hex,
            "
a3 00 01 07 02 03 26 00 01 00 0a 02 01 ff 19 01 b4 0a 0a d9
a3 01 02 c8 14 05 ff ff 00 00 ff ff ff ff ff 00 ff ff 94 12
a3 02 ff 00 14 01 96 00 00 00 00 23 00 02 0f 05 02 01 ff 0a
a3 03 14 01 fb 00 00 01 fa 0a 04 04 ff 00 b4 ff 00 47 ff b3
a3 04 ff e3 ff 00 00 00 00 00 00 00 00 1a 00 00 00 01 02 5d
a3 05 01 ff 05 01 c8 14 14 02 ee 14 01 00 ff 00 00 00 00 92
a3 ff 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 5c
33 05 04 d4 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 e6
"
        );
    }

    fn hex_lines(bytes: &[u8]) -> String {
        let mut hex = String::new();
        for (idx, b) in bytes.iter().enumerate() {
            if idx % 20 == 0 && !hex.is_empty() {
                hex.push('\n');
            } else if !hex.is_empty() {
                hex.push(' ');
            }
            hex.push_str(&format!("{b:02x}"));
        }
        hex
    }

    /// Golden frames generated by the Python reference implementation the
    /// protocol was reverse-engineered with (verified live on H607C):
    /// profile 0x72 (H607C "Rhythm"), a four-colour palette, sensitivity 99.
    #[test]
    fn music_palette_four_colours() {
        let command = SetMusicPalette {
            profile: 0x72,
            colors: vec![
                [0xff, 0x7a, 0x00],
                [0x14, 0x00, 0xc8],
                [0x4a, 0x00, 0xe0],
                [0xff, 0xc2, 0x4a],
            ],
            sensitivity: 99,
        };

        k9::snapshot!(
            hex_lines(&command.encode().unwrap()),
            "
a3 00 01 02 41 72 04 ff 7a 00 14 00 c8 4a 00 e0 ff c2 4a 13
a3 ff 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 5c
33 05 13 72 63 00 00 00 00 00 00 00 00 00 00 00 00 00 00 34
"
        );
    }

    /// Seven colours is the classic-dialect maximum: 21 palette bytes,
    /// split 12 into the first fragment and 9 into the `a3 ff` tail.
    #[test]
    fn music_palette_seven_colours_spills_into_tail() {
        let command = SetMusicPalette {
            profile: 0x33,
            colors: (1..=7u8).map(|i| [i, 2 * i, 3 * i]).collect(),
            sensitivity: 100,
        };

        k9::snapshot!(
            hex_lines(&command.encode().unwrap()),
            "
a3 00 01 02 41 33 07 01 02 03 02 04 06 03 06 09 04 08 0c d9
a3 ff 05 0a 0f 06 0c 12 07 0e 15 00 00 00 00 00 00 00 00 58
33 05 13 33 64 00 00 00 00 00 00 00 00 00 00 00 00 00 00 72
"
        );
    }

    #[test]
    fn music_palette_rejects_bad_input() {
        let base = SetMusicPalette {
            profile: 0x72,
            colors: vec![[255, 0, 0]],
            sensitivity: 99,
        };

        let empty = SetMusicPalette {
            colors: vec![],
            ..base.clone()
        };
        assert!(empty.encode().is_err());

        let too_many = SetMusicPalette {
            colors: vec![[0, 0, 0]; 8],
            ..base.clone()
        };
        assert!(too_many.encode().is_err());

        let over_range = SetMusicPalette {
            sensitivity: 101,
            ..base
        };
        assert!(over_range.encode().is_err());
    }

    #[test]
    fn music_palette_is_registered_for_generic_lights() {
        let command = SetMusicPalette {
            profile: 0x72,
            colors: vec![[255, 0, 0]],
            sensitivity: 99,
        };
        let encoded = Base64HexBytes::encode_for_sku("Generic:Light", &command).unwrap();
        // Three 20-byte frames, base64() splits them per frame for ptReal
        assert_eq!(encoded.base64().len(), 3);
    }

    /// Every fixture below is a verbatim frame from the 2026-09-01
    /// H7180 account-topic capture (wez/govee2mqtt#173); nothing is
    /// synthesized. Comments give the burst context.
    mod rice_cooker {
        use super::*;

        fn decode(frame: &[u8; 20]) -> GoveeBlePacket {
            MGR.decode_for_sku("H7180", frame)
        }

        fn active(program: u8) -> GoveeBlePacket {
            GoveeBlePacket::NotifyRiceCookerActiveProgram(NotifyRiceCookerActiveProgram { program })
        }

        // 07:09:35 status burst: cooker idle.
        const IDLE: [u8; 20] = [
            0xAA, 0x05, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xAF,
        ];
        // 07:09:46 / 07:10:32 / 07:10:15: programs started from the app.
        const PROGRAM_1: [u8; 20] = [
            0xAA, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xAE,
        ];
        const PROGRAM_3: [u8; 20] = [
            0xAA, 0x05, 0x00, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xAC,
        ];
        const PROGRAM_4: [u8; 20] = [
            0xAA, 0x05, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xAB,
        ];

        #[test]
        fn active_program_frames() {
            assert_eq!(decode(&IDLE), active(0));
            assert_eq!(decode(&PROGRAM_1), active(1));
            assert_eq!(decode(&PROGRAM_3), active(3));
            assert_eq!(decode(&PROGRAM_4), active(4));
        }

        #[test]
        fn program_phase_frames() {
            for (frame, program, phase) in [
                // idle, program 1 running, program 4, program 3
                (
                    [
                        0xAA, 0x19, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xB3,
                    ],
                    0,
                    0,
                ),
                (
                    [
                        0xAA, 0x19, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xB0,
                    ],
                    1,
                    2,
                ),
                (
                    [
                        0xAA, 0x19, 0x04, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xB6,
                    ],
                    4,
                    1,
                ),
                (
                    [
                        0xAA, 0x19, 0x03, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xB1,
                    ],
                    3,
                    1,
                ),
            ] {
                assert_eq!(
                    decode(&frame),
                    GoveeBlePacket::NotifyRiceCookerProgramPhase(NotifyRiceCookerProgramPhase {
                        program,
                        phase
                    })
                );
            }
        }

        /// The set temperature moved 131.00°F -> 149.00°F -> 113.00°F
        /// (= 55/65/45°C) as it was changed in the Govee app.
        #[test]
        fn program_1_set_temperature_follows_the_app() {
            // 07:09:46: program 1 running, set temperature 131.00°F
            let f1: [u8; 20] = [
                0xAA, 0x05, 0x01, 0x00, 0x00, 0x00, 0x01, 0x33, 0x2C, 0x02, 0xD0, 0x01, 0x00, 0x2D,
                0x00, 0x00, 0x2D, 0x02, 0x00, 0x61,
            ];
            // 07:09:55: still 149.00°F, but the byte before the
            // temperature blipped 01 -> 00 (meaning unknown); the
            // temperature must still decode.
            let f2: [u8; 20] = [
                0xAA, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x3A, 0x34, 0x02, 0xD0, 0x01, 0x00, 0x2D,
                0x00, 0x00, 0x2D, 0x02, 0x00, 0x71,
            ];
            // 07:10:06: program stopped; residual report with the
            // trailing fields zeroed keeps the last set temperature.
            let f3: [u8; 20] = [
                0xAA, 0x05, 0x01, 0x00, 0x00, 0x00, 0x01, 0x2C, 0x24, 0x02, 0xD0, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x75,
            ];
            for (frame, centi_f) in [(f1, 13100), (f2, 14900), (f3, 11300)] {
                match decode(&frame) {
                    GoveeBlePacket::NotifyRiceCookerProgramParams(p) => {
                        assert_eq!(p.program, 1);
                        assert_eq!(p.set_temperature_centi_fahrenheit(), Some(centi_f));
                    }
                    other => panic!("expected program params, got {other:?}"),
                }
            }
        }

        /// Programs 3 and 4 carry the same `01 <temp> 02 D0` block at
        /// their own offsets, preceded by program-specific bytes whose
        /// meaning is still unknown (0x46=70 for program 3, 0x03/0x78=120
        /// for program 4).
        #[test]
        fn program_3_and_4_set_temperatures() {
            // 07:10:32: program 3, set temperature 113.00°F (45°C)
            let p3: [u8; 20] = [
                0xAA, 0x05, 0x03, 0x00, 0x46, 0x00, 0x00, 0x01, 0x2C, 0x24, 0x02, 0xD0, 0x01, 0x00,
                0x00, 0x00, 0x00, 0x01, 0x00, 0x31,
            ];
            // 07:10:15: program 4, set temperature 131.00°F (55°C)
            let p4: [u8; 20] = [
                0xAA, 0x05, 0x04, 0x03, 0x00, 0x78, 0x00, 0x00, 0x01, 0x33, 0x2C, 0x02, 0xD0, 0x01,
                0x00, 0x00, 0x00, 0x00, 0x01, 0x1C,
            ];
            match decode(&p3) {
                GoveeBlePacket::NotifyRiceCookerProgramParams(p) => {
                    assert_eq!(p.program, 3);
                    assert_eq!(p.set_temperature_centi_fahrenheit(), Some(11300));
                }
                other => panic!("expected program params, got {other:?}"),
            }
            match decode(&p4) {
                GoveeBlePacket::NotifyRiceCookerProgramParams(p) => {
                    assert_eq!(p.program, 4);
                    assert_eq!(p.set_temperature_centi_fahrenheit(), Some(13100));
                }
                other => panic!("expected program params, got {other:?}"),
            }
        }

        /// Frames the capture did not explain stay Generic: the other
        /// status families, a corrupted frame, and the one `AB`-prefixed
        /// ptReal event (which also fails the XOR checksum by 0x02 —
        /// the AB dialect's checksum is an open question).
        #[test]
        fn unexplained_frames_stay_generic() {
            // status families 0x22/0x17 (07:09:35 burst)
            let keep_warm_store: [u8; 20] = [
                0xAA, 0x22, 0x00, 0x33, 0x2C, 0x02, 0xD0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x45,
            ];
            let family_17: [u8; 20] = [
                0xAA, 0x17, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xBD,
            ];
            // PROGRAM_3 with one bit flipped -> checksum no longer matches
            let mut corrupted = PROGRAM_3;
            corrupted[3] = 0x02;
            // 07:09:39 ptReal event frame
            let pt_real: [u8; 20] = [
                0xAB, 0x00, 0x01, 0x00, 0x03, 0x02, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0xAA,
            ];
            for frame in [keep_warm_store, family_17, corrupted, pt_real] {
                assert!(
                    matches!(decode(&frame), GoveeBlePacket::Generic(_)),
                    "{frame:02X?} must not decode"
                );
            }
        }

        /// Unknown program layouts must not fabricate a temperature.
        #[test]
        fn unknown_program_yields_no_temperature() {
            let p = NotifyRiceCookerProgramParams {
                program: 2,
                params: [0xFF; 16],
            };
            assert_eq!(p.set_temperature_centi_fahrenheit(), None);
        }
    }
}
