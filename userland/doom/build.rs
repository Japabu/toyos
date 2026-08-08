use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, path::Path};

/// The upstream doomgeneric commit `/bin/doom` is built from. `forks.toml`
/// records it as the estate's one non-crate third-party source.
///
/// Everything this project measures about doom is a measurement of *these*
/// sources. Fetching `refs/heads/master` instead made which ones a function of
/// the day the fetch happened, and two checkouts could differ with nothing
/// reporting it.
const DOOMGENERIC_COMMIT: &str = "fc601639494e089702a1ada082eb51aaafc03722";

/// Written into the extracted tree, so the pin binds a checkout that already
/// has one. Testing only whether the directory exists is the defect itself: a
/// tree fetched before the pin existed would never be replaced by it.
const PIN_STAMP: &str = ".toyos-commit";

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dg_dir = root.join("doomgeneric");

    let at = fs::read_to_string(dg_dir.join(PIN_STAMP)).ok();
    if at.as_deref().map(str::trim) != Some(DOOMGENERIC_COMMIT) {
        if dg_dir.exists() {
            println!(
                "cargo:warning=doomgeneric/ is at {} and the pin is {DOOMGENERIC_COMMIT}; \
                 replacing it. That directory is gitignored, so anything edited in place \
                 there goes with it.",
                at.as_deref().map(str::trim).unwrap_or("no recorded commit")
            );
            fs::remove_dir_all(&dg_dir).expect("failed to remove the stale doomgeneric tree");
        }
        download_doomgeneric(&root);
        fs::write(dg_dir.join(PIN_STAMP), DOOMGENERIC_COMMIT).expect("failed to record the pin");
    }

    // Use pre-built toyos-cc host binary (built by the build system's toolchain phase).
    let host = std::env::var("HOST").unwrap();
    let toyos_cc = root.join(format!("../../toyos-cc/target/{host}/release/toyos-cc"));
    assert!(
        toyos_cc.exists(),
        "toyos-cc host binary not found at {} — run `cargo run` from repo root first",
        toyos_cc.display()
    );

    let target = std::env::var("TARGET").unwrap();

    let mut build = cc::Build::new();
    build
        .compiler(&toyos_cc)
        .cargo_warnings(false)
        .include("../libc/include")
        .include("include")
        .include("doomgeneric")
        .define("FEATURE_SOUND", None)
        .warnings(false)
        .opt_level(2)
        .flag(&format!("--target={target}"))
        .flag("-include")
        .flag(&root.join("include/doomtype.h").to_string_lossy().into_owned());

    let sources = [
        "am_map.c",
        "d_event.c",
        "d_items.c",
        "d_iwad.c",
        "d_loop.c",
        "d_main.c",
        "d_mode.c",
        "d_net.c",
        "doomdef.c",
        "doomgeneric.c",
        "doomstat.c",
        "dstrings.c",
        "dummy.c",
        "f_finale.c",
        "f_wipe.c",
        "g_game.c",
        "gusconf.c",
        "hu_lib.c",
        "hu_stuff.c",
        "i_endoom.c",
        "i_input.c",
        "i_joystick.c",
        "i_scale.c",
        "i_sound.c",
        "i_system.c",
        "i_timer.c",
        "i_video.c",
        "icon.c",
        "info.c",
        "m_argv.c",
        "m_bbox.c",
        "m_cheat.c",
        "m_config.c",
        "m_controls.c",
        "m_fixed.c",
        "m_menu.c",
        "m_misc.c",
        "m_random.c",
        "memio.c",
        "mus2mid.c",
        "p_ceilng.c",
        "p_doors.c",
        "p_enemy.c",
        "p_floor.c",
        "p_inter.c",
        "p_lights.c",
        "p_map.c",
        "p_maputl.c",
        "p_mobj.c",
        "p_plats.c",
        "p_pspr.c",
        "p_saveg.c",
        "p_setup.c",
        "p_sight.c",
        "p_spec.c",
        "p_switch.c",
        "p_telept.c",
        "p_tick.c",
        "p_user.c",
        "r_bsp.c",
        "r_data.c",
        "r_draw.c",
        "r_main.c",
        "r_plane.c",
        "r_segs.c",
        "r_sky.c",
        "r_things.c",
        "s_sound.c",
        "sha1.c",
        "sounds.c",
        "st_lib.c",
        "st_stuff.c",
        "statdump.c",
        "tables.c",
        "v_video.c",
        "w_checksum.c",
        "w_file.c",
        "w_file_stdc.c",
        "w_main.c",
        "w_wad.c",
        "wi_stuff.c",
        "z_zone.c",
    ];

    for src in &sources {
        build.file(format!("doomgeneric/{src}"));
    }

    build.compile("doomgeneric");

    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=doomgeneric");
}

fn http_agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::WebPki)
        .unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .build()
        .new_agent()
}

fn download_doomgeneric(root: &Path) {
    println!("Downloading doomgeneric {DOOMGENERIC_COMMIT}...");
    let agent = http_agent();
    // The commit is the checksum: GitHub's archive of a sha is that sha's tree
    // and can be nothing else, so pinning the URL needs no second hash of ours.
    let resp = agent
        .get(format!(
            "https://github.com/ozkl/doomgeneric/archive/{DOOMGENERIC_COMMIT}.tar.gz"
        ))
        .call()
        .expect("failed to download doomgeneric");
    let gz = flate2::read::GzDecoder::new(resp.into_body().into_reader());
    let mut archive = tar::Archive::new(gz);
    let dg_dir = root.join("doomgeneric");
    for entry in archive.entries().expect("failed to read archive") {
        let mut entry = entry.expect("failed to read entry");
        let path = entry.path().expect("failed to read path").into_owned();
        // Archive structure: doomgeneric-<commit>/doomgeneric/<files>
        // We want only the doomgeneric/ subdirectory contents.
        let components: Vec<_> = path.components().collect();
        if components.len() < 3 {
            continue;
        }
        // Skip "doomgeneric-master/" and "doomgeneric/" prefixes
        let final_path: PathBuf = components.iter().skip(2).collect();
        if final_path.as_os_str().is_empty() {
            continue;
        }
        // Only extract from the doomgeneric/ subfolder
        let second: PathBuf = components.iter().skip(1).take(1).collect();
        if second.to_str() != Some("doomgeneric") {
            continue;
        }
        let dest = dg_dir.join(&final_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).ok();
        }
        entry.unpack(&dest).expect("failed to unpack entry");
    }
}
