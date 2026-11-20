use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::string::{String, ToString};
use std::{env, format, fs, io, vec::Vec};

/// Generate `params_generated.rs` in the calling application's Cargo output directory.
///
/// Relative CSV paths are resolved against the application's manifest directory.
/// Call this from the application's build script.
pub fn generate_params(csv_path: impl AsRef<Path>) -> io::Result<()> {
    let manifest_dir = cargo_dir("CARGO_MANIFEST_DIR")?;
    let path = manifest_dir.join(csv_path);
    std::println!("cargo:rerun-if-changed={}", path.display());
    let out_dir = cargo_dir("OUT_DIR")?;

    let csv = fs::read_to_string(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    let out = render_params(&csv)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    fs::write(out_dir.join("params_generated.rs"), out)
}

fn render_params(csv: &str) -> io::Result<String> {
    const HEADER: &str = "name,category,param_id,dtype,max_len,persistent,sub_mask,default";
    let header = csv.lines().next().unwrap_or("");
    let columns: Vec<_> = header.split(',').map(str::trim).collect();
    if columns != HEADER.split(',').collect::<Vec<_>>() {
        return Err(invalid(1, format!("expected header: {HEADER}")));
    }
    let mut rows = Vec::new();
    let mut defaults = Vec::new();
    let mut offset = 0usize;

    for (line_num, line) in csv.lines().enumerate().skip(1) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        let line_num = line_num + 1;
        if cols.len() != 8 {
            return Err(invalid(line_num, "expected 8 columns, including default"));
        }

        let name = cols[0].to_string();
        let cat: u8 = parse(cols[1], "category", line_num)?;
        let pid: u16 = parse(cols[2], "param_id", line_num)?;
        let ext_id: u32 = ((cat as u32) << 16) | (pid as u32);
        let dtype = cols[3].to_string();
        let max_len: usize = parse(cols[4], "max_len", line_num)?;
        let persistent: bool = parse(cols[5], "persistent", line_num)?;
        let mask: u16 = if let Some(hex) = cols[6].strip_prefix("0x") {
            u16::from_str_radix(hex, 16).map_err(|_| invalid(line_num, "invalid sub_mask"))?
        } else {
            parse(cols[6], "sub_mask", line_num)?
        };
        let sub_mask = format!("0x{mask:04X}");
        if rows
            .iter()
            .any(|row: &(String, u32, usize, usize, String, bool, String)| {
                row.0 == name || row.1 == ext_id
            })
        {
            return Err(invalid(line_num, "duplicate name or external ID"));
        }
        defaults.extend(default_bytes(&dtype, max_len, cols[7], line_num)?);

        rows.push((name, ext_id, offset, max_len, dtype, persistent, sub_mask));
        offset = offset
            .checked_add(max_len)
            .ok_or_else(|| invalid(line_num, "catalog buffer is too large"))?;
    }

    let count = rows.len();
    if count == 0 {
        return Err(invalid(1, "catalog must contain at least one parameter"));
    }
    let buf_size = offset;
    let words = count.div_ceil(32);

    // Sort a mapping of (ext_id -> dense_index) for binary search in Flash
    let mut ext_map: Vec<(u32, usize)> =
        rows.iter().enumerate().map(|(idx, r)| (r.1, idx)).collect();
    ext_map.sort_by_key(|&(ext_id, _)| ext_id);

    let mut out = String::new();
    out.push_str("use blackboard::{DataType, ParamCatalog, ParamMeta};\n\n");
    out.push_str(&format!("pub const PARAM_COUNT: usize = {};\n", count));
    out.push_str(&format!(
        "pub const PARAM_BUF_SIZE: usize = {};\n",
        buf_size
    ));
    out.push_str(&format!("pub const PARAM_WORDS: usize = {};\n\n", words));
    out.push_str(&format!(
        "pub const PARAM_DEFAULT_BYTES: [u8; PARAM_BUF_SIZE] = {defaults:?};\n\n"
    ));

    // 1. Enum
    out.push_str(
        "#[repr(usize)]\n#[derive(Copy, Clone, PartialEq, Eq, Debug)]\npub enum ParamId {\n",
    );
    for (idx, r) in rows.iter().enumerate() {
        out.push_str(&format!("    {} = {},\n", r.0, idx));
    }
    out.push_str("}\n\n");

    // 2. Flash Metadata Table
    out.push_str(&format!(
        "pub static PARAM_META: [ParamMeta; {}] = [\n",
        count
    ));
    for r in &rows {
        out.push_str(&format!(
            "    ParamMeta {{ ext_id: 0x{:08X}, offset: {}, max_len: {}, dtype: DataType::{}, persistent: {}, sub_mask: {} }},\n",
            r.1, r.2, r.3, r.4, r.5, r.6
        ));
    }
    out.push_str("];\n\n");

    // 3. Sorted External ID Lookup Table (Flash)
    out.push_str(&format!(
        "static EXT_ID_MAP: [(u32, usize); {}] = [\n",
        count
    ));
    for (ext_id, idx) in &ext_map {
        out.push_str(&format!("    (0x{:08X}, {}),\n", ext_id, idx));
    }
    out.push_str("];\n\n");
    out.push_str(&format!("const PARAM_IDS: [ParamId; {count}] = [\n"));
    for row in &rows {
        out.push_str(&format!("    ParamId::{},\n", row.0));
    }
    out.push_str("];\n\n");

    // 4. Trait Implementation
    out.push_str(&format!(
        r#"impl ParamCatalog for ParamId {{
    const COUNT: usize = {count};
    const BUF_SIZE: usize = {buf_size};
    const DEFAULT_BYTES: &'static [u8] = &PARAM_DEFAULT_BYTES;
    #[inline] fn index(self) -> usize {{ self as usize }}
    #[inline] fn from_index(idx: usize) -> Option<Self> {{
        PARAM_IDS.get(idx).copied()
    }}
    #[inline] fn meta(self) -> &'static ParamMeta {{ &PARAM_META[self as usize] }}
    #[inline] fn all_meta() -> &'static [ParamMeta] {{ &PARAM_META }}
    fn from_ext_id(ext_id: u32) -> Option<Self> {{
        EXT_ID_MAP.binary_search_by_key(&ext_id, |&(id, _)| id)
            .ok()
            .and_then(|pos| Self::from_index(EXT_ID_MAP[pos].1))
    }}
}}
"#
    ));

    Ok(out)
}

fn invalid(line: usize, message: impl core::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("line {line}: {message}"),
    )
}

fn parse<T: FromStr>(value: &str, field: &str, line: usize) -> io::Result<T> {
    value
        .parse()
        .map_err(|_| invalid(line, format!("invalid {field}: {value:?}")))
}

/// Scalars use decimal values; Bytes uses `hex:` followed by whitespace-separated
/// byte pairs (or an empty field for all zeros). Bytes are padded to max_len.
fn default_bytes(dtype: &str, width: usize, value: &str, line: usize) -> io::Result<Vec<u8>> {
    let bytes = match dtype {
        "U8" => parse::<u8>(value, "U8 default", line)?
            .to_le_bytes()
            .to_vec(),
        "I8" => parse::<i8>(value, "I8 default", line)?
            .to_le_bytes()
            .to_vec(),
        "U16" => parse::<u16>(value, "U16 default", line)?
            .to_le_bytes()
            .to_vec(),
        "I16" => parse::<i16>(value, "I16 default", line)?
            .to_le_bytes()
            .to_vec(),
        "U32" => parse::<u32>(value, "U32 default", line)?
            .to_le_bytes()
            .to_vec(),
        "I32" => parse::<i32>(value, "I32 default", line)?
            .to_le_bytes()
            .to_vec(),
        "F32" => {
            let number: f32 = parse(value, "F32 default", line)?;
            if !number.is_finite() {
                return Err(invalid(line, "F32 default must be finite"));
            }
            number.to_le_bytes().to_vec()
        }
        "Bool" => std::vec![u8::from(parse::<bool>(value, "Bool default", line)?)],
        "Bytes" => {
            let mut bytes = Vec::new();
            if !value.is_empty() {
                let hex = value
                    .strip_prefix("hex:")
                    .ok_or_else(|| invalid(line, "Bytes default must start with hex:"))?;
                for pair in hex.split_whitespace() {
                    if pair.len() != 2 {
                        return Err(invalid(
                            line,
                            "Bytes default requires two hex digits per byte",
                        ));
                    }
                    bytes.push(
                        u8::from_str_radix(pair, 16)
                            .map_err(|_| invalid(line, "invalid hex byte in default"))?,
                    );
                }
            }
            if width == 0 || bytes.len() > width {
                return Err(invalid(line, "Bytes default must fit a nonempty slot"));
            }
            bytes.resize(width, 0);
            bytes
        }
        _ => return Err(invalid(line, format!("unsupported dtype: {dtype}"))),
    };
    if bytes.len() != width {
        return Err(invalid(
            line,
            format!("{dtype} requires max_len = {}", bytes.len()),
        ));
    }
    Ok(bytes)
}

fn cargo_dir(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Cargo did not set {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "name,category,param_id,dtype,max_len,persistent,sub_mask,default\n";

    #[test]
    fn defaults_encode_every_supported_type_and_pad_byte_slots() {
        for (dtype, value, expected) in [
            ("U8", "255", std::vec![255]),
            ("I8", "-128", std::vec![128]),
            ("U16", "4660", std::vec![0x34, 0x12]),
            ("I16", "-32768", std::vec![0, 128]),
            ("U32", "4294967295", std::vec![255; 4]),
            ("I32", "-2147483648", std::vec![0, 0, 0, 128]),
            ("F32", "1.5", 1.5f32.to_le_bytes().to_vec()),
            ("Bool", "true", std::vec![1]),
            ("Bool", "false", std::vec![0]),
            ("Bytes", "hex:00 aB FF", std::vec![0, 171, 255]),
        ] {
            assert_eq!(
                default_bytes(dtype, expected.len(), value, 2).unwrap(),
                expected
            );
        }
        assert_eq!(
            default_bytes("Bytes", 4, "hex:41 42", 2).unwrap(),
            [65, 66, 0, 0]
        );
        assert_eq!(default_bytes("Bytes", 3, "", 2).unwrap(), [0; 3]);
    }

    #[test]
    fn invalid_defaults_report_the_csv_line() {
        for (dtype, width, value) in [
            ("U8", 1, "256"),
            ("U8", 1, "-1"),
            ("I8", 1, "128"),
            ("U16", 2, "65536"),
            ("I16", 2, "-32769"),
            ("U32", 4, "4294967296"),
            ("I32", 4, "2147483648"),
            ("Bool", 1, "1"),
            ("F32", 4, "NaN"),
            ("F32", 4, "inf"),
            ("F32", 4, "1e100"),
            ("U32", 4, ""),
            ("U32", 2, "1"),
            ("Bytes", 0, ""),
            ("Bytes", 1, "hex:00 01"),
            ("Bytes", 1, "hex:zz"),
            ("Bytes", 1, "hex:F"),
            ("Bytes", 3, "abc"),
            ("U64", 8, "1"),
        ] {
            let csv = format!("{HEADER}Value,1,1,{dtype},{width},false,0x0001,{value}\n");
            let error = render_params(&csv).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("line 2:"), "{error}");
        }
    }

    #[test]
    fn catalog_contains_packed_defaults_in_csv_order() {
        let csv = format!(
            "{HEADER}Speed,1,2,U32,4,false,0x0001,1500\n\
            Enabled,1,1,Bool,1,true,2,true\n\
            Label,1,3,Bytes,4,false,0,hex:41 42\n"
        );
        let output = render_params(&csv).unwrap();
        assert!(output.contains("PARAM_BUF_SIZE: usize = 9;"));
        assert!(output.contains(
            "PARAM_DEFAULT_BYTES: [u8; PARAM_BUF_SIZE] = [220, 5, 0, 0, 1, 65, 66, 0, 0];"
        ));
        assert!(output.contains("const DEFAULT_BYTES: &'static [u8] = &PARAM_DEFAULT_BYTES;"));
        assert!(output.contains("offset: 4, max_len: 1"));
        assert!(output.contains("offset: 5, max_len: 4"));
        assert!(output.contains("(0x00010001, 1),\n    (0x00010002, 0),"));
    }

    #[test]
    fn missing_defaults_and_invalid_catalog_fields_fail_generation() {
        for csv in [
            "name,category,param_id,dtype,max_len,persistent,sub_mask\nValue,1,1,U32,4,false,1\n"
                .to_string(),
            HEADER.to_string(),
            format!("{HEADER}Value,1,1,U32,4,false,1\n"),
            format!("{HEADER}Value,256,1,U32,4,false,1,0\n"),
            format!("{HEADER}Value,1,1,U32,4,no,1,0\n"),
            format!("{HEADER}Value,1,1,U32,4,false,0x10000,0\n"),
            format!("{HEADER}First,1,1,U32,4,false,1,0\nSecond,1,1,U32,4,false,1,1\n"),
        ] {
            assert_eq!(
                render_params(&csv).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
