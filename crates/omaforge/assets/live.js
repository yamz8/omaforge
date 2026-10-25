// Live CI pages: on a job page, append new log output and update step states; on a
// run page, reload until the run finishes. Pages work without this.
(function () {
  var script = document.currentScript;
  var reload = script && script.getAttribute("data-reload");
  if (reload) {
    setTimeout(function () { location.reload(); }, Number(reload));
    return;
  }
  var job = document.getElementById("job");
  var url = job && job.getAttribute("data-live");
  if (!url) return;
  var steps = Array.prototype.slice.call(job.querySelectorAll("details.step"));

  function setState(el, info) {
    if (!el) return;
    el.className = "ci-state " + info["class"];
    el.title = info.label;
    el.innerHTML = info.icon; // Our own SVG, from the server.
  }

  function poll() {
    var offsets = steps.map(function (s) { return s.getAttribute("data-offset") || "0"; }).join(",");
    fetch(url + "?o=" + offsets, { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(r.status); })
      .then(function (data) {
        data.steps.forEach(function (info) {
          var step = steps[info.n];
          if (!step) return;
          var wasRunning = step.querySelector(".ci-state").classList.contains("running");
          setState(step.querySelector("summary .ci-state"), info);
          step.querySelector(".step-duration").textContent = info.duration;
          step.setAttribute("data-offset", info.offset);
          var log = step.querySelector("pre.log");
          var atBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 40;
          info.lines.forEach(function (line) {
            var span = document.createElement("span");
            span.className = "line " + line[0];
            span.textContent = line[1];
            log.appendChild(span);
          });
          if (info["class"] === "running" && !wasRunning) step.open = true;
          if (info.lines.length && atBottom) window.scrollTo(0, document.body.scrollHeight);
        });
        setState(job.querySelector("[data-job-state]"), data);
        if (data.done) {
          setTimeout(function () { location.reload(); }, 800);
        } else {
          setTimeout(poll, 1500);
        }
      })
      .catch(function () { setTimeout(poll, 5000); });
  }
  setTimeout(poll, 1000);
})();
