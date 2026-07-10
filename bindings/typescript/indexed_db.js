const DB_NAME = "hugr-wasm";
const DB_VERSION = 1;

export async function loadSettings() {
  return (await get("settings", "default")) || {};
}

export async function saveSettings(settings) {
  await put("settings", { id: "default", ...settings });
}

export async function listSessions() {
  const sessions = await all("sessions");
  return sessions.sort((a, b) => String(b.createdAt || "").localeCompare(String(a.createdAt || "")));
}

export async function saveSession(session) {
  await put("sessions", session);
}

export async function getSession(traceId) {
  return await get("sessions", traceId);
}

export async function deleteSession(traceId) {
  await del("sessions", traceId);
}

export async function listFiles() {
  return await all("files");
}

export async function saveLocalFile(file) {
  await put("files", file);
}

export async function getLocalFile(fileId) {
  return await get("files", fileId);
}

export async function deleteLocalFile(fileId) {
  await del("files", fileId);
}

async function get(storeName, key) {
  const db = await openDb();
  return await request(db.transaction(storeName).objectStore(storeName).get(key));
}

async function put(storeName, value) {
  const db = await openDb();
  const tx = db.transaction(storeName, "readwrite");
  await request(tx.objectStore(storeName).put(value));
  await done(tx);
}

async function del(storeName, key) {
  const db = await openDb();
  const tx = db.transaction(storeName, "readwrite");
  await request(tx.objectStore(storeName).delete(key));
  await done(tx);
}

async function all(storeName) {
  const db = await openDb();
  return await request(db.transaction(storeName).objectStore(storeName).getAll());
}

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains("settings")) db.createObjectStore("settings", { keyPath: "id" });
      if (!db.objectStoreNames.contains("sessions")) db.createObjectStore("sessions", { keyPath: "traceId" });
      if (!db.objectStoreNames.contains("files")) db.createObjectStore("files", { keyPath: "fileId" });
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function request(req) {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function done(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}
