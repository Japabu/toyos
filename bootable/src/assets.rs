use std::fs;

pub fn collect() -> Vec<(String, Vec<u8>)> {
    let mut files = vec![];

    // Ship the TTF directly — font rasterization happens in userland at runtime
    let ttf = fs::read("assets/JetBrainsMono-Regular.ttf").expect("Failed to read font TTF");
    files.push(("JetBrainsMono-Regular.ttf".to_string(), ttf));

    for entry in fs::read_dir("assets/icons").expect("Failed to read assets/icons") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "svg") {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let name = format!("{stem}.svg");
            let data = fs::read(&path).expect("Failed to read SVG asset");
            files.push((name, data));
        }
    }

    files
}
