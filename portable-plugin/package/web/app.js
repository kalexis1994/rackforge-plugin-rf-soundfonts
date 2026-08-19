(() => {
  "use strict";
  const keyboard = document.querySelector(".keyboard");
  if (keyboard) {
    const blackNotes = new Set([1, 3, 6, 8, 10]);
    for (let note = 0; note < 36; note += 1) {
      const key = document.createElement("i");
      key.className = blackNotes.has(note % 12) ? "black" : "white";
      keyboard.append(key);
    }
  }

  const pending = new Map();
  let requestSerial = 0;
  const hostRequest = (method, params) =>
    new Promise((resolve, reject) => {
      const requestId = `rf-soundfonts-${Date.now()}-${requestSerial += 1}`;
      pending.set(requestId, { resolve, reject });
      window.parent.postMessage({
        protocol: "rackforge.plugin.web@1",
        kind: "request",
        request_id: requestId,
        method,
        params,
      }, window.location.origin);
    });

  window.addEventListener("message", (event) => {
    if (
      event.origin !== window.location.origin ||
      event.source !== window.parent ||
      event.data?.protocol !== "rackforge.plugin.web@1"
    ) return;
    if (event.data.kind === "response") {
      const request = pending.get(event.data.request_id);
      if (!request) return;
      pending.delete(event.data.request_id);
      if (event.data.ok) request.resolve(event.data.result);
      else request.reject(new Error(event.data.error || "RackForge rejected the request."));
    }
  });

  const selectLibrary = document.querySelector("[data-select-library]");
  const selection = document.querySelector("[data-library-selection]");
  const libraryFiles = document.querySelector("[data-library-files]");
  const libraryEntries = document.querySelector("[data-library-entries]");
  const showLibrary = async (grant, parentId = null, ancestors = []) => {
    if (!libraryFiles || !libraryEntries || !grant?.grant_id) return;
    libraryFiles.hidden = false;
    libraryEntries.textContent = "Reading library…";
    try {
      const entries = await hostRequest("plugin.resource_entries", {
        grant_id: grant.grant_id,
        parent_id: parentId,
      });
      libraryEntries.replaceChildren();
      if (parentId) {
        const back = document.createElement("button");
        back.type = "button";
        back.className = "library-entry";
        back.textContent = "‹  Parent folder";
        const previous = ancestors.length ? ancestors[ancestors.length - 1] : null;
        back.addEventListener("click", () =>
          showLibrary(grant, previous, ancestors.slice(0, -1)));
        libraryEntries.append(back);
      }
      // RustySynth rejects SF3 (compressed) banks, so only offer what loads.
      const visible = entries.filter((entry) =>
        entry.kind === "directory" || /\.sf2$/i.test(entry.name));
      if (!visible.length) {
        const empty = document.createElement("span");
        empty.textContent = "No SoundFont files found in this folder.";
        libraryEntries.append(empty);
        return;
      }
      for (const entry of visible) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "library-entry";
        button.textContent = `${entry.kind === "directory" ? "▸" : "♪"}  ${entry.name}`;
        if (entry.kind === "directory") {
          button.addEventListener("click", () =>
            showLibrary(grant, entry.id, [...ancestors, parentId]));
        } else {
          button.addEventListener("click", async () => {
            for (const current of libraryEntries.children) current.disabled = true;
            selection.textContent = `Loading ${entry.name}…`;
            try {
              await hostRequest("plugin.load_resource", {
                target_resource_id: "factory-soundfont",
                grant_id: grant.grant_id,
                entry_id: entry.id,
              });
              for (const current of libraryEntries.children) current.classList.remove("selected");
              button.classList.add("selected");
              selection.textContent = `Playing bank: ${entry.name}`;
            } catch (error) {
              selection.textContent = error instanceof Error ? error.message : "Bank could not be loaded.";
            } finally {
              for (const current of libraryEntries.children) current.disabled = false;
            }
          });
        }
        libraryEntries.append(button);
      }
    } catch (error) {
      libraryEntries.textContent = error instanceof Error ? error.message : "Could not read library.";
    }
  };
  selectLibrary?.addEventListener("click", async () => {
    selectLibrary.disabled = true;
    selection.textContent = "Waiting for RackForge…";
    try {
      const grant = await hostRequest("plugin.select_resource", {
        resource_id: "user-library",
      });
      selection.textContent = grant?.display_name
        ? `Authorized: ${grant.display_name}`
        : "Library authorized.";
      await showLibrary(grant);
    } catch (error) {
      selection.textContent = error instanceof Error ? error.message : "Selection failed.";
    } finally {
      selectLibrary.disabled = false;
    }
  });

  window.parent.postMessage({
    protocol: "rackforge.plugin.web@1",
    kind: "ready",
  }, window.location.origin);

  if (selection) {
    hostRequest("plugin.resource_bindings", {})
      .then((grants) => {
        const grant = grants.find((candidate) => candidate.resource_id === "user-library");
        if (!grant) return;
        selection.textContent = `Authorized: ${grant.display_name}`;
        return showLibrary(grant);
      })
      .catch(() => {
        // A missing grant is the normal first-run state.
      });
  }
})();
