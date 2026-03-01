use cranelift_codegen::isa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

pub fn create_module(output_name: &str, target: Option<&str>) -> ObjectModule {
    let mut flag_builder = settings::builder();
    flag_builder.set("opt_level", "none").unwrap();
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
    let product = module.finish();
    product.emit().unwrap()
}
