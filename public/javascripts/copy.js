document.querySelectorAll("[data-copy]").forEach(function (button) {
  var timer = null;
  button.addEventListener("click", function () {
    var text = document.getElementById(button.dataset.copy).textContent;
    navigator.clipboard.writeText(text).then(function () {
      button.classList.add("copied");
      button.setAttribute("aria-label", "Copied");
      clearTimeout(timer);
      timer = setTimeout(function () {
        button.classList.remove("copied");
        button.setAttribute("aria-label", "Copy");
      }, 1500);
    });
  });
});
