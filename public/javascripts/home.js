(function () {
  var message = document.getElementById("message");
  var length = document.getElementById("length");
  if (message && length) {
    var encoder = new TextEncoder();
    var count = function () {
      length.textContent = encoder.encode(message.value).length + " bytes";
    };
    message.addEventListener("input", count);
    count();
  }

  var file = document.getElementById("file");
  var fileLength = document.getElementById("file-length");
  if (file && fileLength) {
    var showFile = function () {
      var chosen = file.files && file.files[0];
      fileLength.textContent = (chosen ? chosen.size : 0) + " bytes";
    };
    file.addEventListener("change", showFile);
    showFile();
  }

  var strip = document.getElementById("recent");
  if (!strip) return;
  var section = strip.parentElement;
  var prev = document.getElementById("recent-prev");
  var next = document.getElementById("recent-next");
  var page = Number(strip.dataset.page);
  var tiles = strip.querySelectorAll(".block");
  var cursor = tiles.length ? tiles[tiles.length - 1].dataset.id : null;
  var shown = tiles.length;
  var loading = false;
  var done = tiles.length < page;

  // Matches time_ago() in src/web/mod.rs.
  function ago(time) {
    var seconds = Math.max(0, Math.floor(Date.now() / 1000) - time);
    if (seconds < 3600) return Math.max(1, Math.floor(seconds / 60)) + " min ago";
    if (seconds < 86400) {
      var hours = Math.floor(seconds / 3600);
      return hours + (hours === 1 ? " hour ago" : " hours ago");
    }
    var days = Math.floor(seconds / 86400);
    return days + (days === 1 ? " day ago" : " days ago");
  }

  function span(className, text) {
    var element = document.createElement("span");
    element.className = className;
    element.textContent = text;
    return element;
  }

  // Messages are user content, so they are only ever set as text.
  function tile(tx) {
    var link = document.createElement("a");
    link.className = "block g" + (shown++ % 6);
    link.href = "https://mempool.space/tx/" + encodeURIComponent(tx.txid);
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.dataset.id = tx.id;
    var foot = span("foot", "");
    foot.append(span("id", tx.txid.slice(0, 8) + "…"), span("sz", tx.bytes + " B"));
    link.append(
      span("ago", ago(tx.time)),
      tx.message === null ? span("msg hex", tx.hex) : span("msg", tx.message),
      foot
    );
    return link;
  }

  function skeletons() {
    var list = [];
    for (var i = 0; i < 3; i++) {
      var skeleton = document.createElement("div");
      skeleton.className = "block skel";
      skeleton.setAttribute("aria-hidden", "true");
      skeleton.append(document.createElement("span"), document.createElement("span"), document.createElement("span"));
      list.push(skeleton);
    }
    return list;
  }

  function finish() {
    done = true;
    var end = document.createElement("div");
    end.className = "end";
    end.textContent = "No older transactions";
    strip.append(end);
  }

  function loadMore() {
    if (loading || done || cursor === null) return;
    loading = true;
    var placeholders = skeletons();
    strip.append.apply(strip, placeholders);
    fetch("/api/recent?before=" + encodeURIComponent(cursor) + "&limit=" + page)
      .then(function (response) {
        if (!response.ok) throw new Error("status " + response.status);
        return response.json();
      })
      .then(function (rows) {
        placeholders.forEach(function (node) { node.remove(); });
        rows.forEach(function (row) { strip.append(tile(row)); });
        if (rows.length) cursor = rows[rows.length - 1].id;
        if (rows.length < page) finish();
        loading = false;
        update();
      })
      .catch(function (error) {
        // Leave the strip as it is. The next scroll tries again.
        placeholders.forEach(function (node) { node.remove(); });
        console.error(error);
        setTimeout(function () { loading = false; }, 5000);
      });
  }

  function update() {
    var max = strip.scrollWidth - strip.clientWidth;
    prev.disabled = strip.scrollLeft <= 2;
    next.disabled = done && strip.scrollLeft >= max - 2;
    section.style.setProperty("--fade-l", strip.scrollLeft > 2 ? 1 : 0);
    section.style.setProperty("--fade-r", strip.scrollLeft >= max - 2 ? 0 : 1);
    if (max - strip.scrollLeft < strip.clientWidth * 1.5) loadMore();
  }

  strip.addEventListener("scroll", update, { passive: true });
  // Let a normal mouse wheel scroll the strip sideways.
  strip.addEventListener("wheel", function (event) {
    if (Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return;
    var max = strip.scrollWidth - strip.clientWidth;
    if ((event.deltaY > 0 && strip.scrollLeft < max) || (event.deltaY < 0 && strip.scrollLeft > 0)) {
      event.preventDefault();
      strip.scrollLeft += event.deltaY;
    }
  }, { passive: false });
  prev.addEventListener("click", function () {
    strip.scrollBy({ left: -strip.clientWidth * 0.8, behavior: "smooth" });
  });
  next.addEventListener("click", function () {
    strip.scrollBy({ left: strip.clientWidth * 0.8, behavior: "smooth" });
  });
  if (done && tiles.length) finish();
  update();
})();
