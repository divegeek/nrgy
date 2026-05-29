use std::{fs, path::Path, time::Duration};

const REPO_RAW: &str =
    "https://raw.githubusercontent.com/teslamotors/vehicle-command/main/pkg/protocol/protobuf";

// Re-fetch protos from upstream after this interval.  Delete proto/.fetched
// to force an immediate refresh, or run `make clean`.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const PROTOS: &[&str] = &[
    "car_server.proto",
    "common.proto",
    "errors.proto",
    "keys.proto",
    "managed_charging.proto",
    "signatures.proto",
    "universal_message.proto",
    "vcsec.proto",
    "vehicle.proto",
];

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let proto_dir = Path::new(&manifest_dir).join("proto");
    fs::create_dir_all(&proto_dir).expect("failed to create proto cache dir");

    let stamp = proto_dir.join(".fetched");
    let stale = stamp
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().unwrap_or(Duration::MAX) > CACHE_TTL)
        .unwrap_or(true);

    if stale {
        for &proto in PROTOS {
            let url = format!("{REPO_RAW}/{proto}");
            println!("cargo:warning=Downloading {proto} from Tesla vehicle-command repo");
            let content = ureq::get(&url)
                .call()
                .unwrap_or_else(|e| panic!("failed to download {proto}: {e}"))
                .into_body()
                .read_to_string()
                .unwrap_or_else(|e| panic!("failed to read {proto} response: {e}"));
            fs::write(proto_dir.join(proto), &content)
                .unwrap_or_else(|e| panic!("failed to write {proto}: {e}"));
        }
        fs::write(&stamp, "").expect("failed to write fetch timestamp");
    }

    let proto_files: Vec<_> = PROTOS.iter().map(|p| proto_dir.join(p)).collect();

    let fds =
        protox::compile(&proto_files, &[&proto_dir]).expect("failed to compile Tesla protobufs");

    let tesla_proto_dir = Path::new(&manifest_dir).join("src").join("tesla/proto");
    fs::create_dir_all(&tesla_proto_dir).expect("failed to create src/tesla_proto");

    prost_build::Config::new()
        .out_dir(&tesla_proto_dir)
        .compile_fds(fds)
        .expect("prost-build failed");
}
