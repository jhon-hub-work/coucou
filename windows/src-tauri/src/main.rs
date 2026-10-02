// Boo runs without a console window: Boo is the whole UI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    boo_lib::run()
}
