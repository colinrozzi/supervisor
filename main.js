/*
 * This will be the entry point of the project chat.
 * Start this file with the path of the project to be edited.
 *
 * This file will start a webserver, and open a chat interface in the browser.
 * The server will have commands to:
 * - Make a change to the project
 * - Start the project
 *
 * On each change, the project will be versioned.
 * The versioning will be done by creating a new commit in the git repository.
 *
 * If the user wants to start the project, the server will start the current version of the project.
 * Any output from the project will be echoes to the command line.
 * There is more fun things to be done here, like sending the output to the chat interface or publishing it to an event stream, but that is for later.
 */

const express = require("express");
const path = require("path");
const fs = require("fs");
const { spawn } = require("child_process");
const app = express();

const port = process.argv[2] || 3000;
const projectPath = process.argv[3] || "./";

// Set up Express middleware and static file serving
app.use(express.json());
app.use(express.static("public"));

app.post("/make-change", async (req, res) => {
  console.log("Making change");
  const change = req.body.change;
  console.log("Change:", change);
  await makeChange(change);
  res.sendStatus(200);
});

app.get("/start-project", (req, res) => {
  startProject();
  res.sendStatus(200);
});

app.get("/get-info", (req, res) => {
  const projectInfo = JSON.parse(
    fs.readFileSync(path.join(projectPath, "ntwk.json")),
  );
  projectInfo.path = projectPath;
  console.log("Project Info:", projectInfo);
  res.json(projectInfo);
});

// Set up Git functionality
const simpleGit = require("simple-git");
const git = simpleGit(projectPath);

// Function to make a change to the project
async function makeChange(change) {
  console.log("Making change:", change);
  const projectDir = path.resolve(projectPath);

  git.checkIsRepo().then((isRepo) => {
    if (!isRepo) {
      console.error("Not a git repository");
      return;
    }
  });

  // check in the current state of the repository
  git.add(".");
  git.commit("Change before applying new change");

  // Generate prompt for LLM
  const prompt = generatePrompt(change, projectDir);

  console.log("Prompt:", prompt);

  // Make change request to LLM
  const llmResponse = await makeChangeRequest(prompt);

  console.log("LLM Response:", llmResponse);

  // Process LLM response and apply changes
  await processLLMResponse(llmResponse, projectDir);

  console.log("Change applied successfully");
}

// Function to start the project
function startProject() {
  console.log("Starting project");
  const projectDir = path.resolve(projectPath);
  const packageJson = JSON.parse(
    fs.readFileSync(path.join(projectDir, "ntwk.json")),
  );

  if (startScript) {
    const process = spawn(packageJson.start.command, packageJson.start.args, {
      cwd: projectDir,
    });

    process.on("error", (err) => {
      console.error("Failed to start project:", err);
    });

    process.on("close", (code) => {
      console.log(`Project exited with code ${code}`);
    });

    process.stdout.on("data", (data) => {
      console.log(`stdout: ${data}`);
    });

    process.stderr.on("data", (data) => {
      console.error(`stderr: ${data}`);
    });

    console.log("Project started successfully");
  } else {
    console.error("No start script found in package.json");
  }
}

// Set up the HTTP server
const server = app.listen(port, () => {
  console.log(`Server running on port ${port}`);
});

// Upgrade HTTP server to WebSocket server
server.on("upgrade", (request, socket, head) => {
  wss.handleUpgrade(request, socket, head, (ws) => {
    wss.emit("connection", ws, request);
  });
});

// Helper functions for makeChange
function generatePrompt(change, directory) {
  const directoryRepresentation = getDirectoryRepresentation(directory);
  return `Make the following change to the project: ${change}\n\nCurrent project structure:\n${directoryRepresentation}`;
}

function getDirectoryRepresentation(dirPath) {
  let representation = "";
  const files = fs.readdirSync(dirPath);
  files.forEach((file) => {
    if (fs.statSync(path.join(dirPath, file)).isDirectory()) {
      representation += `${file}/\n`;
    } else {
      representation += `${file}\n`;
    }
  });
  return representation;
}

async function processLLMResponse(llmResponse, directory) {
  console.log("Processing LLM response");
  console.log("LLM Response:", llmResponse.toString().substring(0, 100));
  const fileRegex = /<file path="([\s\S]+?)">([\s\S]+?)<\/file>/gs;
  const fileMatches = [...llmResponse.matchAll(fileRegex)];
  console.log("fileMatches", fileMatches);
  fileMatches.forEach((match) => {
    const [, filePath, fileContents] = match;
    console.log("Writing file", filePath);
    if (!fs.existsSync(path.dirname(path.join(directory, filePath)))) {
      fs.mkdirSync(path.dirname(path.join(directory, filePath)), {
        recursive: true,
      });
    }
    fs.writeFileSync(path.join(directory, filePath), fileContents);
  });
}

function start(id) {
  const dirPath = setupActiveDir(id);
  console.log("Starting service", id, "at", dirPath);
  const supervisor_config = JSON.parse(
    fs.readFileSync(path.join(dirPath, "ntwk.json"), "utf-8"),
  );

  console.log("Starting supervisor with config:", supervisor_config);
  console.log("running", supervisor_config.start, supervisor_config.args);
  const freePort = getFreePort();
  const process = spawn(supervisor_config.start, supervisor_config.args, {
    cwd: dirPath,
    env: {
      ...process.env,
      ...supervisor_config.env,
      PORT: freePort,
    },
  });
  console.log("Started process", process.pid);

  process.on("error", (err) => {
    console.error("Failed to start subprocess:", err);
  });

  process.on("close", (code) => {
    console.log(`Child process exited with code ${code}`);
  });

  process.stdout.on("data", (data) => {
    console.log(`stdout: ${data}`);
  });

  process.stderr.on("data", (data) => {
    console.error(`stderr: ${data}`);
  });

  return process;
}

function stop() {
  if (this.process) {
    this.process.kill();
  }
}

// handle sigint (ctrl+c) to stop the supervisor gracefully
process.on("sigint", () => {
  console.log("stopping supervisor...");
  children.foreach((child) => {
    child.process.kill();
  });
  process.exit();
});

//--------------------------------------------------------------------------------------------
// changer.js

const Anthropic = require("@anthropic-ai/sdk");
const anthropic = new Anthropic();

async function makeChangeRequest(prompt) {
  const msg = await anthropic.messages.create({
    model: "claude-3-5-sonnet-20240620",
    max_tokens: 1024,
    messages: [{ role: "user", content: prompt }],
  });
  return msg.content[0].text;
}

//----------------------------------------------------------------

function getDirectoryRepresentation(dirPath) {
  return makeDirectoryRepresentation(dirPath, "");
}

const ignoreFiles = [
  "node_modules",
  ".git",
  ".directory-chat-log",
  "logs",
  ".uploader.lock",
];

function makeDirectoryRepresentation(directory, relative) {
  let curpath = directory;
  if (relative !== "") {
    curpath = `${directory}/${relative}`;
  }
  console.log(curpath);
  const files = fs.readdirSync(`${curpath}/${relative}`);
  let representation = "";
  files.forEach((file) => {
    const type = fs.statSync(`${curpath}/${file}`).isDirectory()
      ? "directory"
      : "file";
    if (type === "directory") {
      if (!ignoreFiles.includes(file)) {
        representation += makeDirectoryRepresentation(
          directory,
          `${relative}/${file}`,
        );
      }
    } else {
      if (!ignoreFiles.includes(file)) {
        const contents = fs.readFileSync(`${curpath}/${file}`).toString();
        representation += `<file path=\"${file}\">${contents}</file>\n`;
      }
    }
  });
  return representation;
}

//----------------------------------------------------------------

async function makeDirectoryChange(outputMatch, directory) {
  // commit the current state of the repository
  const output = outputMatch[1];
  console.log("Output:", output);
  // match the <file name="...">...</file
  const fileRegex = /<file path="([\s\S]+?)">([\s\S]+?)<\/file>/gs;
  const fileMatch = output.match(fileRegex);
  console.log("File Match:", fileMatch);
  // write the files to the irectory
  fileMatch.forEach((file) => {
    const fileRegex = /<file path="([\s\S]+?)">([\s\S]+?)<\/file>/s;
    const fileMatch = file.match(fileRegex);
    console.log("File Match:", fileMatch);
    const relativePath = fileMatch[1];
    const fileContents = fileMatch[2];
    console.log("Relative Path:", relativePath);
    console.log("File Contents:", fileContents);
    fs.writeFileSync(`${directory}/${relativePath}`, fileContents);
  });
}
