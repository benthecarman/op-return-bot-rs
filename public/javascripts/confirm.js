// Asks mempool.space whether the transaction on the success page has
// confirmed, and moves the message into the chain when it has.
(function () {
  var card = document.querySelector("[data-txid]");
  if (!card) return;
  var txid = card.dataset.txid;
  var first = true;
  var timer = null;

  function height(value) {
    return value < 0 ? "" : value.toLocaleString("en-US");
  }

  function confirmed(block, instant) {
    clearInterval(timer);
    document.getElementById("status-title").textContent = "Mined!";
    document.getElementById("status-sub").textContent = "Your message is now permanent.";
    document.getElementById("mined-height").textContent = height(block);
    document.getElementById("mined-step").className = "done mined";
    var captions = card.querySelectorAll(".chainrow .caption");
    captions[0].textContent = height(block - 2);
    captions[1].textContent = height(block - 1);
    captions[2].textContent = "Block " + height(block);
    // A transaction that was already mined skips the animation.
    if (instant) card.classList.add("instant");
    card.classList.add("confirmed");
  }

  function check() {
    var instant = first;
    first = false;
    fetch("https://mempool.space/api/tx/" + encodeURIComponent(txid) + "/status")
      .then(function (response) { return response.ok ? response.json() : null; })
      .then(function (status) {
        if (status && status.confirmed) confirmed(status.block_height, instant);
      })
      .catch(function () {});
  }

  check();
  timer = setInterval(check, 30000);
})();
