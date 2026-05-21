#[cfg(windows)]
use winres::WindowsResource;

fn main() {
    #[cfg(windows)]
    {
        let mut res = WindowsResource::new();
        res.set_icon("logo_app.ico");
        res.compile().unwrap();
    }
    // En macOS / Linux no hay recurso de icono embebido en el binario;
    // egui muestra el icono de la ventana a partir de logo_app.ico (ver main.rs).
}
