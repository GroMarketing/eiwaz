//! Small JSON documents kept in the app's data folder (bench results, savings
//! ledger, per-project settings). Local only; nothing leaves the Mac.

use serde_json::Value;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub fn data_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn doc_path<R: tauri::Runtime>(app: &AppHandle<R>, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("Bad store name".into());
    }
    Ok(data_dir(app)?.join(format!("{name}.json")))
}

#[tauri::command(async)]
pub fn store_read<R: tauri::Runtime>(app: AppHandle<R>, name: String) -> Result<Value, String> {
    let p = doc_path(&app, &name)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        Err(_) => Ok(Value::Null),
    }
}

#[tauri::command(async)]
pub fn store_write<R: tauri::Runtime>(app: AppHandle<R>, name: String, value: Value) -> Result<(), String> {
    let p = doc_path(&app, &name)?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}
