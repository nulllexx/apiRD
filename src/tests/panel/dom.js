'use strict';
/*
 * A DOM small enough to read and large enough to run the admin panel's own
 * JavaScript.
 *
 * `private/rdadmin.html` is a single file with its script inline, so there is
 * nothing to import and no build step to hook. The tests here lift the code
 * out of the page and run it against this, which means they exercise what the
 * browser will actually execute rather than a copy that can drift from it.
 *
 * Deliberately not jsdom: this suite has no package.json and no node_modules,
 * and `cargo test` should not need one. Everything here is stdlib.
 *
 * Supported selector forms are the ones the panel actually uses: tag, #id,
 * .class, [attr], [attr="v"], :checked, :not(.class), and descendant
 * combinators. Anything else needs adding before it will match.
 */

function parseCompound(text) {
    const out = { tag: null, id: null, classes: [], attrs: [], checked: false, not: [] };
    const re = /(:not\([^)]*\))|(\[[^\]]*\])|(:checked)|(#[\w-]+)|(\.[\w-]+)|([\w-]+)/g;
    let m;
    while ((m = re.exec(text))) {
        if (m[1]) out.not.push(m[1].slice(5, -1));
        else if (m[2]) {
            const body = m[2].slice(1, -1);
            const eq = body.indexOf('=');
            if (eq === -1) out.attrs.push([body, null]);
            else out.attrs.push([body.slice(0, eq), body.slice(eq + 1).replace(/^["']|["']$/g, '')]);
        } else if (m[3]) out.checked = true;
        else if (m[4]) out.id = m[4].slice(1);
        else if (m[5]) out.classes.push(m[5].slice(1));
        else if (m[6]) out.tag = m[6].toLowerCase();
    }
    return out;
}

function parseSelector(sel) {
    return sel.trim().split(/\s+/).map(parseCompound);
}

function matchesCompound(el, c) {
    if (c.tag && el.tagName.toLowerCase() !== c.tag) return false;
    if (c.id && el.id !== c.id) return false;
    if (!c.classes.every(cls => el.classList.contains(cls))) return false;
    if (c.checked && !el.checked) return false;
    for (const [name, value] of c.attrs) {
        const actual = el.getAttribute(name);
        if (actual === null || actual === undefined) return false;
        if (value !== null && String(actual) !== value) return false;
    }
    for (const n of c.not) {
        if (matchesCompound(el, parseCompound(n))) return false;
    }
    return true;
}

function matches(el, sel) {
    return sel.split(',').some(part => {
        const chain = parseSelector(part);
        if (!matchesCompound(el, chain[chain.length - 1])) return false;
        let i = chain.length - 2;
        let node = el.parentNode;
        while (i >= 0) {
            if (!node) return false;
            if (node.nodeType === 1 && matchesCompound(node, chain[i])) i--;
            node = node.parentNode;
        }
        return true;
    });
}

class ClassList {
    constructor(el) { this.el = el; }
    _all() { return this.el._class.split(/\s+/).filter(Boolean); }
    _set(list) { this.el._class = list.join(' '); }
    contains(c) { return this._all().includes(c); }
    add(...cs) { const l = this._all(); cs.forEach(c => { if (!l.includes(c)) l.push(c); }); this._set(l); }
    remove(...cs) { this._set(this._all().filter(c => !cs.includes(c))); }
    toggle(c) {
        if (this.contains(c)) { this.remove(c); return false; }
        this.add(c); return true;
    }
}

class TextNode {
    constructor(text) { this.nodeType = 3; this.textContent = text; this.parentNode = null; }
}

class Element {
    constructor(tag) {
        this.nodeType = 1;
        this.tagName = tag.toUpperCase();
        this.childNodes = [];
        this.parentNode = null;
        this._class = '';
        this._attrs = {};
        this._text = null;
        this.style = {};
        this.dataset = {};
        this.hidden = false;
        this.disabled = false;
        this.checked = false;
        this.value = '';
        this._listeners = {};
        this.classList = new ClassList(this);
        // Overridden per-test where clamping matters.
        this.scrollHeight = 0;
        this.clientHeight = 0;
    }

    get className() { return this._class; }
    set className(v) { this._class = v || ''; }

    get id() { return this._attrs.id || ''; }
    set id(v) { this._attrs.id = v; }

    get children() { return this.childNodes.filter(n => n.nodeType === 1); }

    get textContent() {
        if (this._text !== null) return this._text;
        return this.childNodes.map(n => n.nodeType === 3 ? n.textContent : n.textContent).join('');
    }
    set textContent(v) {
        this.childNodes = [];
        this._text = String(v);
    }

    get innerHTML() { return this._html || ''; }
    set innerHTML(v) {
        this.childNodes = [];
        this._text = null;
        this._html = v;
    }

    appendChild(node) {
        if (this._text !== null) { this.childNodes.push(new TextNode(this._text)); this._text = null; }
        node.parentNode = this;
        this.childNodes.push(node);
        return node;
    }

    insertAdjacentElement(where, node) {
        const parent = this.parentNode;
        if (!parent) return null;
        const i = parent.childNodes.indexOf(this);
        node.parentNode = parent;
        parent.childNodes.splice(where === 'afterend' ? i + 1 : i, 0, node);
        return node;
    }

    remove() {
        if (!this.parentNode) return;
        const i = this.parentNode.childNodes.indexOf(this);
        if (i >= 0) this.parentNode.childNodes.splice(i, 1);
        this.parentNode = null;
    }

    setAttribute(k, v) {
        this._attrs[k] = String(v);
        if (k.startsWith('data-')) {
            const key = k.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
            this.dataset[key] = String(v);
        }
    }
    getAttribute(k) {
        if (k in this._attrs) return this._attrs[k];
        if (k === 'class') return this._class || null;
        if (k.startsWith('data-')) {
            const key = k.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
            return key in this.dataset ? this.dataset[key] : null;
        }
        return null;
    }

    _descendants(out = []) {
        for (const n of this.childNodes) {
            if (n.nodeType !== 1) continue;
            out.push(n);
            n._descendants(out);
        }
        return out;
    }

    querySelectorAll(sel) { return this._descendants().filter(el => matches(el, sel)); }
    querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }

    closest(sel) {
        let node = this;
        while (node && node.nodeType === 1) {
            if (matches(node, sel)) return node;
            node = node.parentNode;
        }
        return null;
    }

    addEventListener(type, fn) { (this._listeners[type] ||= []).push(fn); }
    dispatch(type, event = {}) {
        const ev = Object.assign(
            { type, target: this, preventDefault() {}, stopPropagation() {} },
            event
        );
        (this._listeners[type] || []).forEach(fn => fn.call(this, ev));
        return ev;
    }

    focus() { this.ownerDocument && (this.ownerDocument.activeElement = this); }
    scrollIntoView() {}
}

function createDocument() {
    const root = new Element('body');
    const doc = {
        body: root,
        activeElement: null,
        _listeners: {},
        createElement(tag) { const el = new Element(tag); el.ownerDocument = doc; return el; },
        createTextNode(t) { return new TextNode(t); },
        getElementById(id) { return root._descendants().find(el => el.id === id) || null; },
        querySelectorAll(sel) { return root.querySelectorAll(sel); },
        querySelector(sel) { return root.querySelector(sel); },
        addEventListener(type, fn) { (doc._listeners[type] ||= []).push(fn); },
        dispatch(type, event = {}) {
            const ev = Object.assign({ type, preventDefault() {}, stopPropagation() {} }, event);
            (doc._listeners[type] || []).forEach(fn => fn(ev));
        },
    };
    return doc;
}

module.exports = { createDocument, Element, matches };
