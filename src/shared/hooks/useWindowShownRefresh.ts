import { useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ClipboardEntry } from "../types";

interface UseWindowShownRefreshOptions {
  pageSize: number;
  typeFilter: string | null;
  searchActive: boolean;
  onFetched: (entries: ClipboardEntry[]) => void;
}

/**
 * History is fetched once on mount and then maintained incrementally from backend events,
 * so a dropped event would leave the list permanently missing entries.
 * Re-reading the newest page whenever the window reappears bounds that gap to one open/close cycle.
 */
export const useWindowShownRefresh = ({
  pageSize,
  typeFilter,
  searchActive,
  onFetched
}: UseWindowShownRefreshOptions) => {
  const optionsRef = useRef({ pageSize, typeFilter, searchActive, onFetched });

  useEffect(() => {
    optionsRef.current = { pageSize, typeFilter, searchActive, onFetched };
  }, [pageSize, typeFilter, searchActive, onFetched]);

  useEffect(() => {
    const unlisten = listen("main-window-shown", async () => {
      const options = optionsRef.current;
      // Search results are an unpaginated view over a different query, so folding the
      // default list into them would show entries the user did not search for.
      if (options.searchActive) return;

      try {
        const entries = await invoke<ClipboardEntry[]>("get_clipboard_history", {
          limit: options.pageSize,
          offset: 0,
          content_type: options.typeFilter || undefined
        });
        options.onFetched(entries);
      } catch (error) {
        console.error("Failed to re-sync history on window show:", error);
      }
    });

    return () => {
      unlisten.then((off) => off());
    };
  }, []);
};
