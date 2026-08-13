use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

// Windows API のインポート
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, VK_CONTROL, VK_ESCAPE, VK_OEM_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    SendMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_IME_CONTROL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

// 初期状態はオン（true）にしておく
static IS_ACTIVE: AtomicBool = AtomicBool::new(true);
static HOOK_HANDLE: OnceLock<HHOOK> = OnceLock::new();

const IMC_GETCONVERSIONMODE: usize = 1;
const IMC_SETCONVERSIONMODE: usize = 2;

// IME変換モードのビット定義（imm.h の IME_CMODE_*）
const IME_CMODE_NATIVE: u32 = 0x1; // 日本語モード(ひらがな/カタカナ)か
const IME_CMODE_KATAKANA: u32 = 0x2; // カタカナか（NATIVEと併用して判定）
const IME_CMODE_FULLSHAPE: u32 = 0x8; // 全角か

// --- 1. Windows APIによる「入力警察」のコア機能 ---

unsafe fn get_ime_wnd() -> HWND {
    let fg = GetForegroundWindow();
    if fg.0 == 0 {
        return HWND(0);
    }

    // Tauri(WebView2)のように、トップレベルの外枠ウィンドウと実際にキー入力を
    // 受けている内側のウィンドウが別物になっているケースがある(診断ログで確認済み)。
    // AttachThreadInputで対象スレッドの入力状態に一時的に相乗りし、
    // 実際にフォーカスを持つウィンドウ(GetFocus)を取得してそちらを優先する
    let fg_thread = GetWindowThreadProcessId(fg, None);
    let cur_thread = GetCurrentThreadId();
    let attached = AttachThreadInput(cur_thread, fg_thread, true).as_bool();
    let focused = GetFocus();
    if attached {
        let _ = AttachThreadInput(cur_thread, fg_thread, false);
    }

    let target = if focused.0 != 0 { focused } else { fg };
    ImmGetDefaultIMEWnd(target)
}

fn patrol_ime_mode() {
    unsafe {
        let hime = get_ime_wnd();
        if hime.0 == 0 {
            return;
        }

        // 現在のIMEステータスを取得
        let status = SendMessageW(
            hime,
            WM_IME_CONTROL,
            WPARAM(IMC_GETCONVERSIONMODE as usize),
            LPARAM(0),
        );

        let current_mode = status.0 as u32;

        // ビット演算で現在の状態を判定
        let is_native = (current_mode & IME_CMODE_NATIVE) != 0;
        let is_katakana = (current_mode & IME_CMODE_KATAKANA) != 0;
        let is_fullshape = (current_mode & IME_CMODE_FULLSHAPE) != 0;

        let new_mode = if !is_native && is_fullshape {
            // 「日本語入力ではなく、かつ全角（＝全角アルファベット）」→ 半角英数へ
            Some(current_mode & !IME_CMODE_FULLSHAPE)
        } else if is_native && is_katakana {
            // 「日本語入力で、かつカタカナ（全角・半角とも）」→ ひらがなへ強制
            Some((current_mode & !IME_CMODE_KATAKANA) | IME_CMODE_FULLSHAPE)
        } else {
            None
        };

        if let Some(new_mode) = new_mode {
            SendMessageW(
                hime,
                WM_IME_CONTROL,
                WPARAM(IMC_SETCONVERSIONMODE as usize),
                LPARAM(new_mode as isize),
            );
        }
    }
}

fn force_ime_off() {
    unsafe {
        let hime = get_ime_wnd();
        if hime.0 != 0 {
            // 定数 6 (IMC_SETOPENSTATUS) に対して 0 (OFF) を送信し、確実に半角英数(A)にする
            SendMessageW(hime, WM_IME_CONTROL, WPARAM(6 as usize), LPARAM(0 as isize));
        }
    }
}

// --- 2. 警察の任務（バックグラウンド監視＆検問） ---

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code == HC_ACTION as i32 {
        let msg = w_param.0 as u32;
        if (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) && IS_ACTIVE.load(Ordering::Relaxed) {
            let kb = *(l_param.0 as *const KBDLLHOOKSTRUCT);
            let vk_code = kb.vkCode;

            let is_esc = vk_code == VK_ESCAPE.0 as u32;
            let is_ctrl_bracket = vk_code == VK_OEM_4.0 as u32
                && (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;

            if is_esc || is_ctrl_bracket {
                force_ime_off();
            }
        }
    }
    // キーは奪わない: 必ずチェーンを継続する
    CallNextHookEx(None, n_code, w_param, l_param)
}

fn start_police_tasks() {
    // 任務A: パトロール（全角アルファベット撲滅 & カタカナ入力の取り締まり）
    thread::spawn(|| {
        loop {
            if IS_ACTIVE.load(Ordering::Relaxed) {
                patrol_ime_mode();
            }
            thread::sleep(Duration::from_millis(30));
        }
    });

    // 任務B: 検問（ESC / Ctrl+[ のグローバル低レベルフック監視）
    thread::spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("キーボードフックの設置に失敗しました: {e:?}");
                return;
            }
        };
        let _ = HOOK_HANDLE.set(hook);

        // メッセージポンプ（WH_KEYBOARD_LLに必須）
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, HWND(0), 0, 0).0;
            if ret <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook);
    });
}

// --- 3. Tauri本体の起動処理 (v2対応版) ---

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // アプリ起動と同時にバックグラウンド任務を開始！
    start_police_tasks();

    tauri::Builder::default()
        .setup(|app| {
            // メニューアイテムを作成
            let toggle_i = MenuItem::with_id(app, "toggle", "Pause", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            
            // メニューにセット
            let menu = Menu::with_items(app, &[&toggle_i, &quit_i])?;

            // メニューの文字を書き換えるためにクローンを持っておく
            let toggle_i_clone = toggle_i.clone();

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(move |_app, event| match event.id.as_ref() {
                    "toggle" => {
                        // 状態を反転
                        let current = IS_ACTIVE.load(Ordering::Relaxed);
                        IS_ACTIVE.store(!current, Ordering::Relaxed);
                        
                        // v2の書き方でメニューのテキストを動的に変更
                        if current {
                            let _ = toggle_i_clone.set_text("Restart");
                        } else {
                            let _ = toggle_i_clone.set_text("Pause");
                        }
                    }
                    "quit" => {
                        std::process::exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}