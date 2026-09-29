import { invoke } from "@tauri-apps/api/core";

/** Copies `text` to the system clipboard. Goes through the app first: WebKit refuses the web
 *  clipboard API once an `await` has passed since the click, which most copy buttons need. */
export async function copyText(text: string): Promise<void> {
  try {
    await invoke("copy_to_clipboard", { text });
  } catch {
    await navigator.clipboard.writeText(text);
  }
}
