// 取り込み画面。FlightRecorder の DB を File System Access API で読み、/api/imports に送る。
// 登録したファイルの FileHandle は IndexedDB に保存し、次回からは許可を取り直すだけで読み直せる。
// 非対応のブラウザでは <input type=file> に切り替える(選んだファイルはページを開いている間だけ有効)。
//
// 取り込む前に、名前・サイズ・最終更新日時が同じスナップショットが残っていないかを問い合わせる。
// 残っていれば、読み込みをスキップしてそれを開くか、取り込み直すかを選べる。

const HAS_FS = 'showOpenFilePicker' in window;
const fallbackFiles = []; // 非対応ブラウザで選んだ File
let lastFile = null; // 再試行用に、最後に送ったファイルを覚えておく

// ---- IndexedDB(キーは登録時に振る id。同名のファイルを複数登録できるように名前は使わない) ----
let dbPromise = null; // 接続は 1 本を使い回す(操作のたびに開くと、閉じられない接続が溜まる)
function openDb() {
  return (dbPromise ??= new Promise((resolve, reject) => {
    const req = indexedDB.open('shinasadame', 2);
    req.onupgradeneeded = () => {
      const db = req.result;
      // v1 は「現行/旧」をキーにしていた。区別をやめたので作り直す
      if (db.objectStoreNames.contains('handles')) db.deleteObjectStore('handles');
      db.createObjectStore('files');
    };
    req.onsuccess = () => {
      // 別タブが新しい版へ上げるときに邪魔しない
      req.result.onversionchange = () => {
        req.result.close();
        dbPromise = null;
      };
      resolve(req.result);
    };
    req.onerror = () => {
      dbPromise = null;
      reject(req.error);
    };
  }));
}
async function idb(mode, fn) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction('files', mode);
    const req = fn(tx.objectStore('files'));
    tx.oncomplete = () => resolve(req.result);
    tx.onerror = () => reject(tx.error);
  });
}
const putHandle = (id, handle) => idb('readwrite', (s) => s.put(handle, id));
const deleteHandle = (id) => idb('readwrite', (s) => s.delete(id));
async function listHandles() {
  const keys = await idb('readonly', (s) => s.getAllKeys());
  const values = await idb('readonly', (s) => s.getAll());
  return keys.map((id, i) => ({ id, handle: values[i] }));
}

// ---- 表示用 ----
function el(tag, props = {}, ...children) {
  // ファイル名などは利用者の環境由来なので、innerHTML は使わず textContent で入れる
  const node = Object.assign(document.createElement(tag), props);
  node.append(...children);
  return node;
}
const fmtSize = (n) => (n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + ' MB' : Math.ceil(n / 1024) + ' KB');
const fmtTime = (ms) => new Date(ms).toLocaleString('ja-JP', { dateStyle: 'short', timeStyle: 'short' });
const describe = (file) => `${fmtSize(file.size)}・更新 ${fmtTime(file.lastModified)}`;

function setMessage(text) {
  document.getElementById('message').textContent = text || '';
}
function showProgress(...nodes) {
  document.getElementById('progress').replaceChildren(...nodes);
}

async function lookup(file) {
  const q = new URLSearchParams({ name: file.name, size: file.size, mtime: file.lastModified });
  try {
    const res = await fetch('/api/snapshots/lookup?' + q);
    return res.ok ? (await res.json()).snapshot : null;
  } catch {
    return null; // 判定できなければ未取り込み扱い(一覧の描画は止めない)
  }
}

// ---- 登録済みファイルの一覧 ----
async function renderFiles() {
  const root = document.getElementById('files');
  const entries = HAS_FS
    ? await listHandles()
    : fallbackFiles.map((file, i) => ({ id: i, file }));
  if (!entries.length) {
    root.replaceChildren(el('p', { className: 'muted', textContent: 'ファイルが登録されていません。' }));
    return;
  }
  const rows = [];
  for (const entry of entries) {
    const name = entry.handle ? entry.handle.name : entry.file.name;
    const info = el('span', { className: 'muted' });
    const state = el('span', { className: 'state' });
    // 許可が残っているときだけ、中身を読まずにメタデータと取り込み状況を出す
    // (許可の確認はユーザー操作の中でしか出せないので、ここでは requestPermission しない)
    const file = entry.file ?? (await peek(entry.handle));
    if (file) {
      info.textContent = describe(file);
      const snap = await lookup(file);
      if (snap) state.append('取り込み済み ', el('a', { href: `/s/${snap.id}/acceptance`, textContent: `#${snap.id}` }));
      else state.textContent = '未取り込み';
    } else {
      info.textContent = '(取り込むときに読み取りの許可を確認します)';
    }

    const importBtn = el('button', { type: 'button', textContent: '取り込む' });
    importBtn.addEventListener('click', () => startImport(entry));
    const row = el('div', { className: 'file-row' }, el('strong', { textContent: name }), info, state, importBtn);
    if (entry.handle) {
      const removeBtn = el('button', { type: 'button', className: 'secondary', textContent: '登録解除' });
      removeBtn.addEventListener('click', async () => {
        await deleteHandle(entry.id);
        renderFiles();
      });
      row.append(removeBtn);
    }
    rows.push(row);
  }
  root.replaceChildren(...rows);
}

async function peek(handle) {
  try {
    if ((await handle.queryPermission({ mode: 'read' })) !== 'granted') return null;
    return await handle.getFile();
  } catch {
    return null;
  }
}

// 取り込みボタンのクリックの中で呼ぶこと(requestPermission はユーザー操作が必要)
async function readFile(entry) {
  if (entry.file) return entry.file;
  const handle = entry.handle;
  if ((await handle.queryPermission({ mode: 'read' })) !== 'granted') {
    if ((await handle.requestPermission({ mode: 'read' })) !== 'granted') {
      throw new Error('ファイルの読み取りが許可されませんでした');
    }
  }
  return handle.getFile();
}

async function registerFile() {
  try {
    const [handle] = await window.showOpenFilePicker({
      types: [{ description: 'SQLite', accept: { 'application/vnd.sqlite3': ['.db'] } }],
    });
    // 同じファイルを二重に登録しない
    for (const { handle: h } of await listHandles()) {
      if (await h.isSameEntry(handle)) {
        setMessage(`${handle.name} は登録済みです。`);
        return;
      }
    }
    await putHandle(crypto.randomUUID(), handle);
    setMessage('');
    renderFiles();
  } catch (e) {
    if (e.name !== 'AbortError') setMessage(e.message);
  }
}

// ---- 取り込み ----
async function startImport(entry) {
  setMessage('');
  let file;
  try {
    file = await readFile(entry);
  } catch (e) {
    setMessage(e.message);
    return;
  }
  const snap = await lookup(file);
  if (!snap) {
    upload(file);
    return;
  }
  // 取り込み済みのデータが残っている: 読み込みをスキップして開くか、取り込み直すかを選ぶ
  const open = el('a', { href: `/s/${snap.id}/acceptance`, textContent: `#${snap.id} を開く` });
  const again = el('button', { type: 'button', className: 'secondary', textContent: '取り込み直す' });
  again.addEventListener('click', () => upload(file));
  showProgress(
    el(
      'div',
      { className: 'progress' },
      `${file.name}(${describe(file)})は取り込み済みです(${snap.imported_at.slice(0, 16).replace('T', ' ')} UTC)。 `,
      open,
      ' ',
      again,
    ),
  );
}

async function upload(file) {
  lastFile = file;
  setMessage('');
  showProgress(el('div', { className: 'progress', textContent: 'アップロード中…' }));
  const body = new FormData();
  body.append('file', file, file.name);
  body.append('file_mtime', String(file.lastModified));
  let res;
  try {
    res = await fetch('/api/imports', { method: 'POST', body });
  } catch (e) {
    showProgress();
    setMessage('アップロードに失敗しました: ' + e.message);
    return;
  }
  if (!res.ok) {
    showProgress();
    setMessage(res.status === 413 ? 'ファイルが大きすぎます(上限 256MB)。' : `アップロードに失敗しました (${res.status})`);
    return;
  }
  const { import_id } = await res.json();
  htmx.ajax('GET', `/imports/${import_id}/status`, { target: '#progress', swap: 'innerHTML' });
}

// 失敗表示の「再試行」から呼ばれる(_import_status.html)
function retryImport() {
  if (lastFile) upload(lastFile);
}

// ---- 初期化 ----
const add = document.getElementById('add-file');
if (HAS_FS) {
  add.replaceChildren(el('button', { type: 'button', className: 'secondary', textContent: 'ファイルを登録', onclick: registerFile }));
} else {
  const input = el('input', { type: 'file', accept: '.db' });
  input.addEventListener('change', () => {
    if (input.files[0]) fallbackFiles.push(input.files[0]);
    input.value = '';
    renderFiles();
  });
  add.replaceChildren(input);
}
renderFiles();
