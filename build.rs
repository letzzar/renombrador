use winres::WindowsResource;

fn main() {
    let mut res = WindowsResource::new();
    res.set_icon("logo_app.ico");
    res.compile().unwrap();
}
