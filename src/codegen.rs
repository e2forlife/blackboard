use std::path::{Path, PathBuf};
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
    let mut rows = Vec::new();
    let mut offset = 0usize;

    for (line_num, line) in csv.lines().enumerate().skip(1) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        assert_eq!(
            cols.len(),
            7,
            "Invalid column count on line {}",
            line_num + 1
        );

        let name = cols[0].to_string();
        let cat: u8 = cols[1].parse().unwrap();
        let pid: u16 = cols[2].parse().unwrap();
        let ext_id: u32 = ((cat as u32) << 16) | (pid as u32);
        let dtype = cols[3].to_string();
        let max_len: usize = cols[4].parse().unwrap();
        let persistent: bool = cols[5].parse().unwrap();
        let sub_mask = cols[6].to_string();

        rows.push((name, ext_id, offset, max_len, dtype, persistent, sub_mask));
        offset += max_len;
    }

    let count = rows.len();
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

    fs::write(out_dir.join("params_generated.rs"), out)
}

fn cargo_dir(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Cargo did not set {name}")))
}
