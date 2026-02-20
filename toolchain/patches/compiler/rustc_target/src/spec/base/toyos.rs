use crate::spec::{Cc, LinkerFlavor, Lld, RelocModel, StackProbeType, TargetOptions};

pub(crate) fn opts() -> TargetOptions {
    TargetOptions {
        os: "toyos".into(),
        linker: Some("rust-lld".into()),
        linker_flavor: LinkerFlavor::Gnu(Cc::No, Lld::Yes),
        stack_probes: StackProbeType::Inline,
        relocation_model: RelocModel::Pic,
        position_independent_executables: true,
        dynamic_linking: false,
        has_thread_local: false,
        main_needs_argc_argv: false,
        ..Default::default()
    }
}
