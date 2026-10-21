(function () {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const app = $("app");
  const topbar = $("topbar");
  const saveForm = $("save-form");
  const urlInput = $("url");
  const saveBtn = $("save-btn");
  const searchInput = $("search");
  const list = $("list");
  const listEmpty = $("list-empty");
  const listMore = $("list-more");
  const listCount = $("list-count");
  const footCount = $("foot-count");
  const sortBtn = $("sort-btn");
  const pinBtn = $("pin-btn");
  const minBtn = $("min-btn");
  const closeBtn = $("close-btn");
  const empty = $("empty");
  const reader = $("reader");
  const readerTitle = $("reader-title");
  const readerHost = $("reader-host");
  const readerDate = $("reader-date");
  const readerHttp = $("reader-http");
  const mobileBtn = $("mobile-btn");
  const openOriginal = $("open-original");
  const deleteBtn = $("delete-btn");
  const page = $("page");
  const loading = $("loading");
  const status = $("status");
  const toast = $("toast");
  const modal = $("modal");
  const modalTitle = $("modal-title");
  const modalText = $("modal-text");
  const modalInput = $("modal-input");
  const modalCancel = $("modal-cancel");
  const modalConfirm = $("modal-confirm");
  const sidebarBtn = $("sidebar-btn");

  const CONTENT_PREFIX = "https://win-rere-content.example/";
  // Keep the DOM bounded no matter how many bookmarks are saved. Search
  // remains the way to reach the rest.
  const MAX_VISIBLE = 50;

  const state = {
    items: [],
    activeId: null,
    topmost: true,
    saving: false,
    query: "",
    sort: "newest",
    sidebarVisible: true,
    mobileView: false,
  };

  let toastTimer = 0;
  let modalMode = null;
  let searchTimer = 0;

  function send(message) {
    try {
      if (window.chrome && window.chrome.webview) {
        window.chrome.webview.postMessage(message);
      }
    } catch (_error) {
      // The host is gone; nothing useful can be done from here.
    }
  }

  function setStatus(message) {
    status.textContent = message;
  }

  function showToast(message, isError) {
    toast.textContent = message;
    toast.classList.toggle("error", !!isError);
    toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () {
      toast.hidden = true;
    }, 6500);
  }

  function setSaving(value) {
    state.saving = value;
    app.classList.toggle("saving", value);
    saveBtn.disabled = value;
    saveBtn.textContent = value ? "SAVING" : "SAVE";
  }

  function setSidebar(visible) {
    state.sidebarVisible = visible;
    app.dataset.sidebar = visible ? "visible" : "hidden";
    sidebarBtn.classList.toggle("active", visible);
    sidebarBtn.setAttribute("aria-pressed", visible ? "true" : "false");
    sidebarBtn.title = visible ? "Hide bookmark list" : "Show bookmark list";
    sidebarBtn.setAttribute("aria-label", sidebarBtn.title);
  }

  function formatBytes(bytes) {
    if (!bytes || bytes < 0) return "";
    if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
    if (bytes >= 1024) return Math.round(bytes / 1024) + " KB";
    return bytes + " B";
  }

  function formatDate(value) {
    const date = new Date(value);
    if (isNaN(date.getTime())) return "";
    return date.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
  }

  function tone(id) {
    let total = 0;
    for (let index = 0; index < id.length; index += 1) {
      total = (total + id.charCodeAt(index)) % 5;
    }
    return total;
  }

  function findItem(id) {
    return state.items.find(function (item) {
      return item.id === id;
    }) || null;
  }

  function searchKey(item) {
    return [item.title, item.host, item.url, item.description]
      .join("\n")
      .toLowerCase();
  }

  function visibleItems() {
    const query = state.query.trim().toLowerCase();
    let items = state.items.slice();
    if (query) {
      items = items.filter(function (item) {
        return (item.search || searchKey(item)).indexOf(query) !== -1;
      });
    }
    if (state.sort === "title") {
      items.sort(function (a, b) {
        return (a.title || "").localeCompare(b.title || "", undefined, { sensitivity: "base" });
      });
    }
    // NEWEST needs no sort: the backend sends the library newest-first and
    // filtering preserves that order.
    return items;
  }

  function makeBookmark(item) {
    const row = document.createElement("div");
    row.className = "bookmark";
    row.dataset.id = item.id;
    row.setAttribute("role", "option");
    row.tabIndex = 0;
    if (item.id === state.activeId) {
      row.classList.add("active");
      row.setAttribute("aria-selected", "true");
    }

    const icon = document.createElement("span");
    icon.className = "favicon tone-" + tone(item.id);
    icon.textContent = (item.host || "?").charAt(0).toUpperCase();

    const body = document.createElement("span");
    body.className = "bm-body";

    const title = document.createElement("span");
    title.className = "bm-title";
    title.textContent = item.title || "Untitled page";
    title.title = item.title || "";

    const sub = document.createElement("span");
    sub.className = "bm-sub";
    const date = formatDate(item.saved_at);
    sub.textContent = date ? item.host + " / " + date : item.host;

    body.appendChild(title);
    body.appendChild(sub);

    const size = document.createElement("span");
    size.className = "bm-size";
    size.textContent = formatBytes(item.bytes);
    size.title = size.textContent;

    const edit = document.createElement("button");
    edit.type = "button";
    edit.className = "bm-edit";
    edit.title = "Edit title";
    edit.setAttribute("aria-label", "Edit title");
    edit.innerHTML =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20h4L18.5 9.5a2.1 2.1 0 0 0-3-3L5 17v3z"/><path d="M13.5 6.5l4 4"/></svg>';
    edit.addEventListener("click", function (event) {
      event.stopPropagation();
      openRenameModal(item);
    });
    edit.addEventListener("keydown", function (event) {
      event.stopPropagation();
    });

    row.appendChild(icon);
    row.appendChild(body);
    row.appendChild(size);
    row.appendChild(edit);
    row.addEventListener("click", function () {
      send({ cmd: "open", id: item.id });
    });
    row.addEventListener("keydown", function (event) {
      if (event.target !== row) return;
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        send({ cmd: "open", id: item.id });
      }
    });
    return row;
  }

  function renderList() {
    const items = visibleItems();
    let shown = items.slice(0, MAX_VISIBLE);
    // The active page must remain visible even when its row sits past the cap.
    if (state.activeId) {
      const activeIndex = items.findIndex(function (item) {
        return item.id === state.activeId;
      });
      if (activeIndex >= MAX_VISIBLE) {
        shown = [items[activeIndex]].concat(shown.slice(0, MAX_VISIBLE - 1));
      }
    }

    list.textContent = "";
    for (const item of shown) {
      list.appendChild(makeBookmark(item));
    }

    const total = state.items.length;
    const hidden = items.length - shown.length;
    listCount.textContent = total + " SAVED";
    footCount.textContent = total + (total === 1 ? " page" : " pages");

    if (items.length === 0) {
      listEmpty.hidden = false;
      if (total === 0) {
        listEmpty.textContent =
          "No bookmarks yet. Paste a page URL above to save the first one.";
      } else {
        listEmpty.textContent = "No bookmarks match that search.";
      }
    } else {
      listEmpty.hidden = true;
    }

    if (hidden > 0) {
      const query = state.query.trim();
      const noun = query
        ? hidden === 1
          ? "match"
          : "matches"
        : hidden === 1
          ? "bookmark"
          : "bookmarks";
      const pronoun = hidden === 1 ? "it" : "them";
      listMore.hidden = false;
      listMore.textContent =
        hidden +
        " more " +
        noun +
        (query
          ? ". Keep typing to narrow the results."
          : ". Search to find " + pronoun + ".");
    } else {
      listMore.hidden = true;
      listMore.textContent = "";
    }
  }

  function updatePin() {
    pinBtn.classList.toggle("active", state.topmost);
    pinBtn.setAttribute("aria-pressed", state.topmost ? "true" : "false");
    pinBtn.title = state.topmost ? "Always on top: on" : "Always on top: off";
  }

  function updateMobile() {
    const item = state.activeId ? findItem(state.activeId) : null;
    const available = !!(item && item.has_mobile);
    const active = available && state.mobileView;
    reader.classList.toggle("mobile", active);
    mobileBtn.classList.toggle("active", active);
    mobileBtn.disabled = !available;
    mobileBtn.setAttribute("aria-pressed", active ? "true" : "false");
    mobileBtn.title = !available
      ? "No mobile snapshot for this page"
      : active ? "Mobile view: on" : "Mobile view: off";
  }

  function loadActiveVariant() {
    const item = state.activeId ? findItem(state.activeId) : null;
    if (!item) return;
    const mobile = state.mobileView && item.has_mobile;
    page.src =
      CONTENT_PREFIX +
      (mobile ? "mobile.html" : "index.html") +
      "?v=" +
      encodeURIComponent(item.id);
    loading.hidden = false;
    setStatus(mobile ? "Opening the mobile copy..." : "Opening the desktop copy...");
  }

  function showReader(item) {
    reader.hidden = false;
    empty.hidden = true;
    readerTitle.textContent = item.title || "Untitled page";
    readerTitle.title = item.title || "";
    readerHost.textContent = item.host || "";
    readerDate.textContent = formatDate(item.saved_at);
    readerHttp.hidden = !item.insecure;
    openOriginal.disabled = false;
    deleteBtn.disabled = false;
    updateMobile();
  }

  function hideReader() {
    reader.hidden = true;
    empty.hidden = false;
    loading.hidden = true;
    state.activeId = null;
    updateMobile();
    try {
      page.src = "about:blank";
    } catch (_error) {
      // Ignore; the frame is being reset.
    }
  }

  function scrollActiveIntoView() {
    const active = list.querySelector(".bookmark.active");
    if (active) {
      active.scrollIntoView({ block: "nearest" });
    }
  }

  function onLibrary(event) {
    state.items = (Array.isArray(event.items) ? event.items : []).map(function (item) {
      item.search = searchKey(item);
      return item;
    });
    state.topmost = !!event.topmost;
    state.mobileView = !!event.mobile_view;
    state.activeId = typeof event.active === "string" ? event.active : null;
    updatePin();
    updateMobile();
    renderList();

    if (state.activeId) {
      const item = findItem(state.activeId);
      if (item) {
        showReader(item);
        scrollActiveIntoView();
      }
    } else if (!state.saving) {
      empty.hidden = false;
      reader.hidden = true;
    }
  }

  function onEvent(event) {
    if (!event || typeof event.event !== "string") return;
    switch (event.event) {
      case "library":
        onLibrary(event);
        break;
      case "save_started":
        setSaving(true);
        setStatus("Downloading page...");
        break;
      case "save_progress":
        setSaving(true);
        setStatus(typeof event.message === "string" ? event.message : "Working...");
        break;
      case "save_done": {
        const item = event.item;
        setSaving(false);
        if (item && item.id) {
          item.search = searchKey(item);
          const existing = findItem(item.id);
          if (existing) {
            existing.search = item.search;
          } else {
            state.items.unshift(item);
          }
          showToast(
            item.has_mobile
              ? "Saved desktop and mobile: " + (item.title || item.host)
              : "Saved desktop only: " + (item.title || item.host)
          );
          renderList();
        }
        setStatus(
          item && item.has_mobile
            ? "Saved offline, desktop and mobile."
            : "Saved offline, desktop copy only."
        );
        urlInput.value = "";
        break;
      }
      case "save_error":
        setSaving(false);
        showToast(typeof event.message === "string" ? event.message : "The page could not be saved.", true);
        setStatus("Save failed.");
        break;
      case "opening": {
        if (typeof event.id !== "string" || typeof event.url !== "string") break;
        if (event.url.indexOf(CONTENT_PREFIX) !== 0) break;
        const item = findItem(event.id);
        state.activeId = event.id;
        showReader(
          item || {
            id: event.id,
            title: "",
            host: "",
            saved_at: "",
            insecure: false,
          }
        );
        renderList();
        scrollActiveIntoView();
        loading.hidden = false;
        setStatus("Opening the offline copy...");
        page.src = event.url;
        break;
      }
      case "opened":
        if (event.id === state.activeId) {
          loading.hidden = true;
          const item = findItem(state.activeId);
          if (item) setStatus("Offline copy of " + item.host);
        }
        break;
      case "open_error":
        loading.hidden = true;
        hideReader();
        renderList();
        showToast(typeof event.message === "string" ? event.message : "The offline copy could not be opened.", true);
        setStatus("Could not open the offline copy.");
        break;
      case "cleared":
        hideReader();
        renderList();
        break;
      case "deleted":
        if (event.id === state.activeId) {
          hideReader();
        }
        showToast("Offline copy deleted.");
        setStatus("Offline copy deleted.");
        break;
      case "delete_error":
        showToast(typeof event.message === "string" ? event.message : "The offline copy could not be deleted.", true);
        break;
      case "renamed":
        showToast("Title updated.");
        setStatus("Bookmark title updated.");
        break;
      case "rename_error":
        showToast(typeof event.message === "string" ? event.message : "The title could not be saved.", true);
        setStatus("Could not rename the bookmark.");
        break;
      case "link_blocked":
        showToast("External links are blocked in offline mode. Use Open original instead.");
        setStatus("External link blocked.");
        break;
      case "pinned":
        state.topmost = !!event.value;
        updatePin();
        setStatus(state.topmost ? "Always on top: on." : "Always on top: off.");
        break;
      case "status":
        if (typeof event.message === "string") setStatus(event.message);
        break;
      default:
        break;
    }
  }

  window.__winRere = { onEvent: onEvent };

  saveForm.addEventListener("submit", function (event) {
    event.preventDefault();
    if (state.saving) return;
    const url = urlInput.value.trim();
    if (!url) {
      urlInput.focus();
      return;
    }
    setSaving(true);
    setStatus("Starting download...");
    send({ cmd: "save", url: url });
  });

  searchInput.addEventListener("input", function () {
    state.query = searchInput.value;
    clearTimeout(searchTimer);
    searchTimer = setTimeout(renderList, 90);
  });

  searchInput.addEventListener("keydown", function (event) {
    if (event.key === "Escape") {
      if (searchInput.value) {
        searchInput.value = "";
        state.query = "";
        clearTimeout(searchTimer);
        renderList();
      } else {
        searchInput.blur();
      }
    } else if (event.key === "ArrowDown") {
      clearTimeout(searchTimer);
      renderList();
      const first = list.querySelector(".bookmark");
      if (first) {
        event.preventDefault();
        first.focus();
      }
    }
  });

  listMore.addEventListener("click", function () {
    searchInput.focus();
    searchInput.select();
  });

  sortBtn.addEventListener("click", function () {
    state.sort = state.sort === "newest" ? "title" : "newest";
    sortBtn.textContent = state.sort === "newest" ? "NEWEST" : "A-Z";
    renderList();
  });

  pinBtn.addEventListener("click", function () {
    send({ cmd: "pin", value: !state.topmost });
  });

  minBtn.addEventListener("click", function () {
    send({ cmd: "minimize" });
  });

  closeBtn.addEventListener("click", function () {
    send({ cmd: "close" });
  });

  openOriginal.addEventListener("click", function () {
    if (state.activeId) send({ cmd: "open_external", id: state.activeId });
  });

  mobileBtn.addEventListener("click", function () {
    const item = state.activeId ? findItem(state.activeId) : null;
    if (!item || !item.has_mobile) return;
    state.mobileView = !state.mobileView;
    updateMobile();
    send({ cmd: "mobile_view", value: state.mobileView });
    loadActiveVariant();
  });

  function closeModal() {
    modal.hidden = true;
    modalMode = null;
  }

  function openDeleteModal(item) {
    modalMode = { kind: "delete", id: item.id };
    modalTitle.textContent = "Delete this offline copy?";
    modalText.textContent =
      'This removes "' + item.title + '" and every file stored with it. The original page is not affected.';
    modalInput.hidden = true;
    modalInput.value = "";
    modalConfirm.textContent = "DELETE";
    modalConfirm.classList.add("danger");
    modal.hidden = false;
    modalConfirm.focus();
  }

  function openRenameModal(item) {
    modalMode = { kind: "rename", id: item.id };
    modalTitle.textContent = "Rename bookmark";
    modalText.textContent = "Only the title changes. The saved page and its URL stay the same.";
    modalInput.hidden = false;
    modalInput.value = item.title || "";
    modalConfirm.textContent = "SAVE";
    modalConfirm.classList.remove("danger");
    modal.hidden = false;
    modalInput.focus();
    modalInput.select();
  }

  deleteBtn.addEventListener("click", function () {
    const item = state.activeId ? findItem(state.activeId) : null;
    if (item) openDeleteModal(item);
  });

  sidebarBtn.addEventListener("click", function () {
    setSidebar(!state.sidebarVisible);
    send({ cmd: "sidebar", visible: state.sidebarVisible });
  });

  modalCancel.addEventListener("click", closeModal);

  modalConfirm.addEventListener("click", function () {
    if (!modalMode) {
      closeModal();
      return;
    }
    const mode = modalMode;
    if (mode.kind === "delete") {
      closeModal();
      setStatus("Deleting the offline copy...");
      send({ cmd: "delete", id: mode.id });
      return;
    }
    const title = modalInput.value.trim();
    if (!title) {
      showToast("A title is required.", true);
      modalInput.focus();
      return;
    }
    closeModal();
    send({ cmd: "rename", id: mode.id, title: title });
  });

  modalInput.addEventListener("keydown", function (event) {
    if (event.key === "Enter") {
      event.preventDefault();
      modalConfirm.click();
    }
  });

  modal.addEventListener("click", function (event) {
    if (event.target === modal) closeModal();
  });

  topbar.addEventListener("mousedown", function (event) {
    if (event.button !== 0) return;
    if (event.target.closest("input, button")) return;
    send({ cmd: "drag" });
    event.preventDefault();
  });

  page.addEventListener("load", function () {
    if (state.activeId) loading.hidden = true;
  });

  document.addEventListener("keydown", function (event) {
    if (!modal.hidden) {
      if (event.key === "Escape") {
        event.preventDefault();
        closeModal();
      }
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      searchInput.focus();
      searchInput.select();
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "l") {
      event.preventDefault();
      urlInput.focus();
      urlInput.select();
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "b") {
      event.preventDefault();
      setSidebar(!state.sidebarVisible);
      send({ cmd: "sidebar", visible: state.sidebarVisible });
    }
  });

  setSidebar(app.dataset.sidebar !== "hidden");
  renderList();
  updatePin();
  send({ cmd: "ready" });
})();
