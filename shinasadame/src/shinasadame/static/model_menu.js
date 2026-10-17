// 「試す」パネルのモデル欄(select[data-cascade])を、複数段のメニュー(会社 ▸ 系列 ▸ モデル)に差し替える。
// 元の select は残して隠し、選んだ値を select.value に入れて change を発火する(フォームはそのまま送れる)。
// JS が動かなければ、会社ごとの <optgroup> 付きの select のまま使える。
//
// 操作: クリック / Enter / Space で開く。↑↓で移動、→ で子を開く、← と Esc で閉じる、外側のクリックで閉じる。
// 子のメニューは position: fixed で出す(親のメニューが縦にスクロールしても切れない)。右端に収まらなければ左に開く。

// select の <option> / <optgroup> を、{label, value} の葉と {label, children} の枝の木にする。
// 系列(data-series)が無いモデルは、会社の直下の葉になる。
function modelMenuTree(select) {
  const root = [];
  for (const node of select.children) {
    if (node.tagName === 'OPTION') {
      root.push({ label: node.textContent.trim(), value: node.value });
    } else if (node.tagName === 'OPTGROUP') {
      const group = { label: node.label, children: [] };
      const seriesNodes = new Map();
      for (const opt of node.querySelectorAll('option')) {
        // 会社の下では、`provider/` を省いて短く見せる(送られる値は変えない)
        const prefix = `${group.label}/`;
        const text = opt.textContent.trim();
        const leaf = { label: text.startsWith(prefix) ? text.slice(prefix.length) : text, value: opt.value };
        const series = opt.dataset.series;
        if (!series) {
          group.children.push(leaf);
          continue;
        }
        if (!seriesNodes.has(series)) {
          const branch = { label: series, children: [] };
          seriesNodes.set(series, branch);
          group.children.push(branch); // 最初に現れた位置に置く
        }
        seriesNodes.get(series).children.push(leaf);
      }
      root.push(group);
    }
  }
  return root;
}

function modelMenuItems(menu) {
  return [...menu.children].filter((li) => li.classList.contains('cmenu-item'));
}

function modelMenuCloseSubs(menu) {
  for (const li of modelMenuItems(menu)) {
    const sub = li.querySelector(':scope > .cmenu');
    if (!sub) continue;
    sub.hidden = true;
    li.setAttribute('aria-expanded', 'false');
    modelMenuCloseSubs(sub);
  }
}

// 親の項目 li の横に、子のメニューを置く。右に収まらなければ左、下に収まらなければ上へずらす。
function modelMenuPlace(li, sub) {
  sub.hidden = false;
  sub.style.maxHeight = `${window.innerHeight - 16}px`;
  const rect = li.getBoundingClientRect();
  let left = rect.right - 2;
  if (left + sub.offsetWidth > window.innerWidth - 8) left = Math.max(8, rect.left - sub.offsetWidth + 2);
  let top = rect.top - 4;
  if (top + sub.offsetHeight > window.innerHeight - 8) top = Math.max(8, window.innerHeight - 8 - sub.offsetHeight);
  sub.style.left = `${left}px`;
  sub.style.top = `${top}px`;
  li.setAttribute('aria-expanded', 'true');
}

function modelMenuBuild(items, isSub) {
  const ul = document.createElement('ul');
  ul.className = isSub ? 'cmenu cmenu-sub' : 'cmenu';
  ul.setAttribute('role', 'menu');
  if (isSub) ul.hidden = true;
  for (const item of items) {
    const li = document.createElement('li');
    li.className = 'cmenu-item';
    li.setAttribute('role', 'menuitem');
    li.tabIndex = -1;
    const label = document.createElement('span');
    label.textContent = item.label;
    li.append(label);
    if (item.children) {
      li.setAttribute('aria-haspopup', 'true');
      li.setAttribute('aria-expanded', 'false');
      const arrow = document.createElement('span');
      arrow.className = 'cmenu-arrow';
      arrow.textContent = '▸';
      li.append(arrow, modelMenuBuild(item.children, true));
    } else {
      li.dataset.value = item.value;
    }
    ul.append(li);
  }
  return ul;
}

function modelMenuInit(select) {
  const wrap = document.createElement('div');
  wrap.className = 'cascade';
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'cascade-button';
  button.setAttribute('aria-haspopup', 'menu');
  button.setAttribute('aria-expanded', 'false');
  const root = modelMenuBuild(modelMenuTree(select), false);
  root.hidden = true;
  wrap.append(button, root);
  select.after(wrap);
  select.hidden = true;

  function refreshButton() {
    button.textContent = `${select.value} ▾`;
    for (const leaf of wrap.querySelectorAll('.cmenu-item[data-value]')) {
      leaf.classList.toggle('cmenu-selected', leaf.dataset.value === select.value);
    }
  }

  function open() {
    root.hidden = false;
    button.setAttribute('aria-expanded', 'true');
    modelMenuItems(root)[0]?.focus({ preventScroll: true }); // 開いた直後のスクロールで scroll リスナーが閉じないように
  }

  function close() {
    root.hidden = true;
    modelMenuCloseSubs(root);
    button.setAttribute('aria-expanded', 'false');
  }

  function pick(value) {
    select.value = value;
    select.dispatchEvent(new Event('change', { bubbles: true }));
    refreshButton();
    close();
    button.focus();
  }

  function openSub(li) {
    const sub = li.querySelector(':scope > .cmenu');
    if (!sub) return null;
    modelMenuCloseSubs(li.parentElement); // 兄弟の子(とその先)を閉じてから開く
    modelMenuPlace(li, sub);
    return sub;
  }

  button.addEventListener('click', () => (root.hidden ? open() : close()));
  button.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (root.hidden) open();
    }
  });

  root.addEventListener('mouseover', (e) => {
    const li = e.target.closest('.cmenu-item');
    if (!li || !root.contains(li)) return;
    if (li.getAttribute('aria-haspopup')) openSub(li);
    else modelMenuCloseSubs(li.parentElement); // 子の無い項目に乗ったら、兄弟の子を閉じる
  });

  root.addEventListener('click', (e) => {
    const li = e.target.closest('.cmenu-item');
    if (!li) return;
    if (li.dataset.value !== undefined) pick(li.dataset.value);
    else openSub(li);
  });

  root.addEventListener('keydown', (e) => {
    const li = e.target.closest('.cmenu-item');
    if (!li) return;
    const siblings = modelMenuItems(li.parentElement);
    const i = siblings.indexOf(li);
    switch (e.key) {
      case 'ArrowDown':
        siblings[(i + 1) % siblings.length].focus();
        break;
      case 'ArrowUp':
        siblings[(i - 1 + siblings.length) % siblings.length].focus();
        break;
      case 'ArrowRight': {
        const sub = openSub(li);
        if (sub) modelMenuItems(sub)[0]?.focus();
        break;
      }
      case 'ArrowLeft':
      case 'Escape': {
        const parentUl = li.parentElement;
        if (parentUl === root) {
          close();
          button.focus();
        } else {
          const parentLi = parentUl.parentElement;
          parentUl.hidden = true;
          parentLi.setAttribute('aria-expanded', 'false');
          parentLi.focus();
        }
        break;
      }
      case 'Enter':
      case ' ': {
        if (li.dataset.value !== undefined) {
          pick(li.dataset.value);
        } else {
          const sub = openSub(li);
          if (sub) modelMenuItems(sub)[0]?.focus();
        }
        break;
      }
      case 'Tab':
        close();
        return;
      default:
        return;
    }
    e.preventDefault();
  });

  document.addEventListener('click', (e) => {
    if (!wrap.contains(e.target)) close();
  });
  window.addEventListener('resize', close);
  window.addEventListener('scroll', close, { passive: true });
  select.addEventListener('change', refreshButton);
  refreshButton();
}

document.querySelectorAll('select[data-cascade]').forEach(modelMenuInit);
