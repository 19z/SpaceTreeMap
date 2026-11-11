//! Shell COM runs only on the UI (STA) thread. Menu and subclass lifetimes cover
//! TrackPopupMenuEx's nested message loop, including third-party owner-drawn menus.
use crate::scanner::wide;
use std::{marker::PhantomData, path::Path, rc::Rc};
use windows::{
    core::{Interface, PCSTR, PCWSTR, PSTR},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
        UI::{Input::KeyboardAndMouse::GetKeyState, Shell::*, WindowsAndMessaging::*},
    },
};

pub struct Apartment {
    initialized: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Apartment {
    pub fn new() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }
            .map_err(|e| format!("无法初始化 Shell STA：{e}"))?;
        Ok(Self {
            initialized: true,
            _thread: PhantomData,
        })
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn ensure_path(path: &Path) -> Result<(), String> {
    path.try_exists()
        .map_err(|e| format!("无法检查 {}：{e}", path.display()))?
        .then_some(())
        .ok_or_else(|| {
            format!(
                "路径已移动、删除或不可访问：{}。请重新扫描。",
                path.display()
            )
        })
}

pub fn open(hwnd: HWND, path: &Path) -> Result<(), String> {
    ensure_path(path)?;
    let path = wide(path.as_os_str());
    let result = unsafe {
        ShellExecuteW(
            Some(hwnd),
            windows::core::w!("open"),
            PCWSTR(path.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        Err(format!("打开失败，Shell 错误码 {}", result.0 as isize))
    } else {
        Ok(())
    }
}

pub enum MenuOutcome {
    Cancelled,
    Invoked,
    Rename,
}
struct Pidl(*mut Common::ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe {
            ILFree(Some(self.0));
        }
    }
}
struct Popup(HMENU);
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}
struct MenuMessages {
    menu3: Option<IContextMenu3>,
    menu2: Option<IContextMenu2>,
}
struct Subclass(HWND);
const SUBCLASS_ID: usize = 0x53544d;
impl Drop for Subclass {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.0, Some(menu_proc), SUBCLASS_ID);
        }
    }
}

unsafe extern "system" fn menu_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    if matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let state = &*(data as *const MenuMessages);
        if let Some(menu) = &state.menu3 {
            let mut result = LRESULT(0);
            if menu
                .HandleMenuMsg2(message, w, l, Some(&mut result))
                .is_ok()
            {
                return result;
            }
        }
        if let Some(menu) = &state.menu2 {
            if menu.HandleMenuMsg(message, w, l).is_ok() {
                return LRESULT(0);
            }
        }
    }
    DefSubclassProc(hwnd, message, w, l)
}

pub fn context_menu(hwnd: HWND, path: &Path) -> Result<MenuOutcome, String> {
    ensure_path(path)?;
    if hwnd.0.is_null() {
        return Err("无法获取窗口句柄".into());
    }
    unsafe { show_menu(hwnd, path) }
        .map_err(|e| format!("Shell 菜单失败（{}）：{e}", path.display()))
}

unsafe fn show_menu(hwnd: HWND, path: &Path) -> windows::core::Result<MenuOutcome> {
    let path_w = wide(path.as_os_str());
    let mut raw_pidl = std::ptr::null_mut();
    SHParseDisplayName(PCWSTR(path_w.as_ptr()), None, &mut raw_pidl, 0, None)?;
    let pidl = Pidl(raw_pidl);
    let mut child = std::ptr::null_mut();
    let parent: IShellFolder = SHBindToParent(pidl.0, Some(&mut child))?;
    let menu: IContextMenu = parent.GetUIObjectOf(hwnd, &[child], None)?;
    let popup = Popup(CreatePopupMenu()?);
    let state = MenuMessages {
        menu3: menu.cast().ok(),
        menu2: menu.cast().ok(),
    };
    SetWindowSubclass(
        hwnd,
        Some(menu_proc),
        SUBCLASS_ID,
        &state as *const _ as usize,
    )
    .ok()?;
    let _subclass = Subclass(hwnd);
    let extended = (GetKeyState(0x10) as u16 & 0x8000) != 0;
    menu.QueryContextMenu(
        popup.0,
        0,
        1,
        0x7fff,
        CMF_EXPLORE | CMF_CANRENAME | if extended { CMF_EXTENDEDVERBS } else { 0 },
    )
    .ok()?;
    let mut point = POINT::default();
    GetCursorPos(&mut point)?;
    let command = TrackPopupMenuEx(
        popup.0,
        (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
        point.x,
        point.y,
        hwnd,
        None,
    )
    .0;
    if command == 0 {
        return Ok(MenuOutcome::Cancelled);
    }
    let ordinal = (command - 1) as usize;
    let mut verb = [0u16; 128];
    if menu
        .GetCommandString(
            ordinal,
            GCS_VERBW,
            None,
            PSTR(verb.as_mut_ptr().cast()),
            verb.len() as u32,
        )
        .is_ok()
        && String::from_utf16_lossy(&verb)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("rename")
    {
        return Ok(MenuOutcome::Rename);
    }
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_PTINVOKE,
        hwnd,
        lpVerb: PCSTR(ordinal as *const u8),
        lpVerbW: PCWSTR(ordinal as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: point,
        ..Default::default()
    };
    menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast())?;
    Ok(MenuOutcome::Invoked)
}

pub fn rename(path: &Path, name: &str) -> Result<(), String> {
    ensure_path(path)?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c < ' ' || "\\/:*?\"<>|".contains(c))
    {
        return Err("请输入有效的文件名，不可包含路径或 Windows 保留字符。".into());
    }
    let target = path.with_file_name(name);
    if target == path {
        return Ok(());
    }
    if target.try_exists().map_err(|e| e.to_string())? {
        return Err("目标名称已存在。".into());
    }
    std::fs::rename(path, &target).map_err(|e| format!("重命名失败：{e}"))
}
