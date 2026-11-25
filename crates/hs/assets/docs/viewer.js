// The docs service's viewer: every page of a PDF drawn by pdf.js, with the
// passage the link highlights boxed where it sits on the page.
//
// The page that loads this is served under a content security policy that
// takes scripts, styles, images and fetches from its own origin only.
// Nothing here writes markup or a style attribute: elements are built with
// the DOM and positioned through their style objects, which the policy
// allows.

import {
  GlobalWorkerOptions,
  RenderingCancelledException,
  getDocument,
} from "./pdf.min.mjs";

GlobalWorkerOptions.workerSrc = new URL(
  "./pdf.worker.min.mjs",
  import.meta.url,
).href;

// Most canvas pixels per CSS pixel. A denser screen gains nothing a reader
// can see, and a page's bitmap grows with the square of it.
const MAX_PIXEL_RATIO = 2;
// How far outside the window a page is drawn before it is scrolled to, and
// kept after it is scrolled away. Past it the bitmap is given back, so a
// long book holds a few pages of pixels, not all of them.
const RENDER_MARGIN = "150% 0px";
const PERCENT = 100;

// What a page's canvas holds.
const EMPTY = "empty";
const DRAWING = "drawing";
const DRAWN = "drawn";
const FAILED = "failed";

// The link's own data, written into the page by the service: the signed
// URL of the PDF, the page the link names, the shape to highlight on each
// page and how many units a page side is divided into.
const config = JSON.parse(document.getElementById("hs-cite").textContent);
const status = document.getElementById("status");
const container = document.getElementById("pages");

function say(message) {
  status.textContent = message;
  status.hidden = false;
}

function shape(box, size) {
  box.style.aspectRatio = `${size.width} / ${size.height}`;
}

// Put a mark where a rectangle `[x0, y0, x1, y1]` lies on its page, in
// fractions of the page: it is right before the page is drawn and at
// whatever width the window gives it.
function place(mark, [x0, y0, x1, y1]) {
  mark.style.left = `${(x0 / config.units) * PERCENT}%`;
  mark.style.top = `${(y0 / config.units) * PERCENT}%`;
  mark.style.width = `${((x1 - x0) / config.units) * PERCENT}%`;
  mark.style.height = `${((y1 - y0) / config.units) * PERCENT}%`;
}

// A page's highlight: the pieces of its `fill` tinted together, so where
// two meet is tinted once, and a line along each piece of its `outline`,
// which is the edge of the whole passage. The lines are laid on together
// too, so a corner, where two of them meet, is no darker than the rest.
function highlightLayer(region) {
  const layer = document.createElement("div");
  layer.className = "highlight";
  layer.append(marks("tint", region.fill), marks("line", region.outline));
  return layer;
}

// One of a highlight's two layers: a mark placed for each rectangle.
function marks(name, rects) {
  const group = document.createElement("div");
  group.className = name;
  for (const rect of rects) {
    const mark = document.createElement("div");
    place(mark, rect);
    group.append(mark);
  }
  return group;
}

// A page's place in the document: a box of the page's shape, a canvas to
// draw it on, and the highlight, when the page has one, laid over both.
function pageBox(number, size, highlight) {
  const box = document.createElement("section");
  box.className = "page";
  box.dataset.page = String(number);
  box.setAttribute("aria-label", `Page ${number}`);
  shape(box, size);
  box.append(document.createElement("canvas"));
  if (highlight) {
    box.append(highlightLayer(highlight));
  }
  return box;
}

function blank(canvas) {
  canvas.width = 0;
  canvas.height = 0;
}

// Draw a page that has come within reach of the window.
async function draw(pdf, entry) {
  if (entry.state !== EMPTY) {
    return;
  }
  entry.state = DRAWING;
  let outcome = EMPTY;
  try {
    const page = await pdf.getPage(entry.number);
    // The page may have been scrolled away while it was being read.
    if (entry.wanted) {
      const size = page.getViewport({ scale: 1 });
      shape(entry.box, size);
      const ratio = Math.min(window.devicePixelRatio || 1, MAX_PIXEL_RATIO);
      const viewport = page.getViewport({
        scale: (entry.box.clientWidth * ratio) / size.width,
      });
      entry.canvas.width = Math.floor(viewport.width);
      entry.canvas.height = Math.floor(viewport.height);
      entry.task = page.render({
        canvasContext: entry.canvas.getContext("2d"),
        viewport,
      });
      await entry.task.promise;
      outcome = DRAWN;
    }
  } catch (error) {
    // A cancelled drawing is a page scrolled away, not a fault.
    if (!(error instanceof RenderingCancelledException)) {
      outcome = FAILED;
      say(`Page ${entry.number} could not be drawn.`);
      console.error(error);
    }
  }
  entry.task = null;
  entry.state = outcome;
  if (outcome !== DRAWN) {
    blank(entry.canvas);
  }
  // The page came back within reach while its drawing was being cancelled.
  if (outcome === EMPTY && entry.wanted) {
    draw(pdf, entry);
  }
}

// Give back the bitmap of a page that has left the window's reach.
function release(entry) {
  entry.wanted = false;
  if (entry.task) {
    entry.task.cancel();
  } else if (entry.state === DRAWN) {
    blank(entry.canvas);
    entry.state = EMPTY;
  }
}

async function show() {
  let pdf;
  try {
    pdf = await getDocument({ url: config.source, isEvalSupported: false })
      .promise;
  } catch (error) {
    say(
      "The document could not be loaded. A link stops working when it expires: ask for a new one.",
    );
    console.error(error);
    return;
  }

  const highlights = new Map(
    config.highlights.map((highlight) => [highlight.page, highlight]),
  );
  // Every page is given the first page's shape until it is drawn: reading
  // each page's own would mean reading the whole document before showing
  // any of it.
  const first = (await pdf.getPage(1)).getViewport({ scale: 1 });
  const entries = [];
  for (let number = 1; number <= pdf.numPages; number += 1) {
    const box = pageBox(number, first, highlights.get(number));
    container.append(box);
    entries.push({
      number,
      box,
      canvas: box.firstElementChild,
      state: EMPTY,
      wanted: false,
      task: null,
    });
  }
  status.hidden = true;

  // Where the reader is taken: the first highlighted passage, or the page
  // the link names. That page gets its own shape first, so the passage is
  // where the scroll expects it.
  const mark = container.querySelector(".highlight .tint div");
  const landing = mark
    ? entries[Number(mark.closest(".page").dataset.page) - 1]
    : entries[(config.page ?? 0) - 1];
  if (landing) {
    const page = await pdf.getPage(landing.number);
    shape(landing.box, page.getViewport({ scale: 1 }));
    if (mark) {
      mark.scrollIntoView({ block: "center" });
    } else {
      landing.box.scrollIntoView({ block: "start" });
    }
  }

  const reach = new IntersectionObserver(
    (changes) => {
      for (const change of changes) {
        const entry = entries[Number(change.target.dataset.page) - 1];
        if (change.isIntersecting) {
          entry.wanted = true;
          draw(pdf, entry);
        } else {
          release(entry);
        }
      }
    },
    { rootMargin: RENDER_MARGIN },
  );
  for (const entry of entries) {
    reach.observe(entry.box);
  }
}

show().catch((error) => {
  say("The document could not be shown.");
  console.error(error);
});
