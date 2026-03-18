use cranelift_codegen::isa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

pub fn create_module(output_name: &str, target: Option<&str>, opt_level: u8) -> ObjectModule {
    let mut flag_builder = settings::builder();
    let opt = match opt_level {
        0 => "none",
        1 => "speed",
        2 => "speed",
        _ => "speed_and_size",
    };
    flag_builder.set("opt_level", opt).unwrap();
    flag_builder.set("is_pic", "true").unwrap();
    let flags = settings::Flags::new(flag_builder);

    let triple: Triple = match target {
        Some(t) => t.parse().unwrap_or_else(|e| panic!("bad target triple '{t}': {e}")),
        None => Triple::host(),
    };
    let isa = isa::lookup(triple.clone())
        .unwrap_or_else(|e| panic!("failed to look up ISA for {triple}: {e}"))
        .finish(flags)
        .unwrap();

    let builder = ObjectBuilder::new(isa, output_name, cranelift_module::default_libcall_names()).unwrap();
    ObjectModule::new(builder)
}

pub fn finish(module: ObjectModule) -> Vec<u8> {
    use cranelift_object::object::SectionKind;
    let mut product = module.finish();
    // Mark this object as needing the C standard library. toyos-ld checks
    // for this section and automatically links libtoyos_c when present.
    let section = product.object.add_section(
        Vec::new(),
        b".note.toyos.libc".to_vec(),
        SectionKind::Note,
    );
    product.object.set_section_data(section, vec![0], 1);
    product.object.write().unwrap()
}
