#![cfg(feature = "build-tools")]

use std::{fs, process::Command};

#[test]
fn application_builds_and_runs_with_csv_defaults() {
    // Exercise the public build-script API and compile its output in a consumer
    // crate. This also checks host build-tools/runtime no_std feature separation.
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = project.join("target").join("csv-defaults-consumer");
    fs::create_dir_all(fixture.join("src")).unwrap();
    let dependency_path = project.to_str().unwrap().replace('\\', "/");
    fs::write(
        fixture.join("Cargo.toml"),
        format!(
            r#"[package]
name = "csv-defaults-consumer"
version = "0.0.0"
edition = "2024"

[workspace]
resolver = "3"

[dependencies]
blackboard = {{ path = {dependency_path:?} }}

[build-dependencies]
blackboard = {{ path = {dependency_path:?}, features = ["build-tools"] }}

[dev-dependencies]
critical-section = {{ version = "1", features = ["std"] }}
"#
        ),
    )
    .unwrap();
    fs::write(
        fixture.join("build.rs"),
        "fn main() -> std::io::Result<()> { blackboard::codegen::generate_params(\"params.csv\") }",
    )
    .unwrap();
    fs::write(
        fixture.join("params.csv"),
        "name,category,param_id,dtype,max_len,persistent,sub_mask,default\n\
         Speed,1,1,U32,4,true,0x0001,1500\n\
         Enabled,1,2,Bool,1,true,0x0001,true\n\
         Gain,1,3,F32,4,false,0x0001,1.5\n\
         Label,1,4,Bytes,5,false,0x0001,hex:41 42\n\
         SmallUnsigned,1,5,U8,1,false,0x0001,255\n\
         SmallSigned,1,6,I8,1,false,0x0001,-128\n\
         MediumUnsigned,1,7,U16,2,false,0x0001,4660\n\
         MediumSigned,1,8,I16,2,false,0x0001,-32768\n\
         LargeSigned,1,9,I32,4,false,0x0001,-2147483648\n",
    )
    .unwrap();
    fs::write(
        fixture.join("src/lib.rs"),
        r#"#![no_std]
include!(concat!(env!("OUT_DIR"), "/params_generated.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use blackboard::{Blackboard, PublisherId};
    use core::future::Future;
    use core::task::{Context, Poll, Waker};

    #[test]
    fn csv_defaults_restore_and_final_notification() {
        static BOARD: Blackboard<ParamId, PARAM_BUF_SIZE, PARAM_WORDS, 1> = Blackboard::new();
        assert_eq!(BOARD.read::<u32>(ParamId::Speed), Some(1500));
        assert_eq!(BOARD.read::<bool>(ParamId::Enabled), Some(true));
        assert_eq!(BOARD.read::<f32>(ParamId::Gain), Some(1.5));
        assert_eq!(BOARD.read::<[u8; 5]>(ParamId::Label), Some([65, 66, 0, 0, 0]));
        assert_eq!(BOARD.read::<u8>(ParamId::SmallUnsigned), Some(255));
        assert_eq!(BOARD.read::<i8>(ParamId::SmallSigned), Some(-128));
        assert_eq!(BOARD.read::<u16>(ParamId::MediumUnsigned), Some(4660));
        assert_eq!(BOARD.read::<i16>(ParamId::MediumSigned), Some(-32768));
        assert_eq!(BOARD.read::<i32>(ParamId::LargeSigned), Some(-2147483648));
        assert_eq!(ParamId::from_ext_id(0x10002), Some(ParamId::Enabled));

        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(core::pin::pin!(BOARD.next_change(0)).poll(&mut cx), Poll::Pending);
        assert!(BOARD.publish(ParamId::Speed, 2500u32, PublisherId::NvmRestore));
        assert_eq!(core::pin::pin!(BOARD.next_change(0)).poll(&mut cx), Poll::Pending);
        BOARD.finish_startup();
        assert_eq!(core::pin::pin!(BOARD.next_change(0)).poll(&mut cx), Poll::Ready(ParamId::Speed));
        assert_eq!(BOARD.read::<u32>(ParamId::Speed), Some(2500));
        assert_eq!(BOARD.read::<bool>(ParamId::Enabled), Some(true));
    }
}
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["test", "--offline", "--manifest-path"])
        .arg(fixture.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(fixture.join("target"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "consumer build/test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
