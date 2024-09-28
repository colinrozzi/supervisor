console.log("Hello from client.js!");

let info;

async function getInfo() {
  info = await fetch("http://localhost:3000/get-info").then((res) =>
    res.json(),
  );
  console.log("info", info);
}

function drawInfo() {
  const infoContainer = document.getElementById("info-container");
  infoContainer.innerHTML = "";
  Object.keys(info).forEach((element) => {
    const div = document.createElement("div");
    if (typeof info[element] === "object") {
      div.innerHTML = `<p>${element}: ${JSON.stringify(info[element])}</p>`;
    } else {
      div.innerHTML = `<p>${element}: ${info[element]}</p>`;
    }
    infoContainer.appendChild(div);
  });
}

document.getElementById("send").addEventListener("click", async () => {
  console.log("clicked");
  const change = document.getElementById("change").value;
  console.log(change);
  await fetch("http://localhost:3000/make-change", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ change }),
  });
});

document.addEventListener("DOMContentLoaded", async () => {
  await getInfo();
  drawInfo();
});
