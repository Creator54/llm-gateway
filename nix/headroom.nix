{ lib
, stdenv
, python3
, rustPlatform
, pkg-config
, onnxruntime
, src
}:

let
  py = python3.pkgs;

  # mcp >= 1.30.0 (< 2.0.0) pinned per uv.lock (1.30.0)
  mcp_1_30 = py.buildPythonPackage rec {
    pname = "mcp";
    version = "1.30.0";
    format = "pyproject";

    src = py.fetchPypi {
      inherit pname version;
      hash = "sha256-RFQUYl/OXClfqlBbsRus7OZhq29AKNV8k121eCC3o+Q=";
    };

    nativeBuildInputs = [
      py.hatchling
      py.uv-dynamic-versioning
    ];

    propagatedBuildInputs = [
      py.anyio
      py.httpx
      py.httpx-sse
      py.jsonschema
      py.pydantic
      py.pydantic-settings
      py.pyjwt
      py.python-multipart
      py.sse-starlette
      py.starlette
      py.typing-extensions
      py.typing-inspection
      py.uvicorn
    ];

    doCheck = false;
  };

  # tree-sitter-language-pack < 1.0 pinned per uv.lock (0.13.0)
  tree-sitter-language-pack_0_13 = py.buildPythonPackage rec {
    pname = "tree_sitter_language_pack";
    version = "0.13.0";
    format = "pyproject";

    src = py.fetchPypi {
      inherit pname version;
      hash = "sha256-AyA0xeJ7H24AcwuefC28ggO0cA0MaB/QGdbe/PYRg+w=";
    };

    nativeBuildInputs = [
      py.setuptools
      py.wheel
      py.cython
      py.typing-extensions
    ];

    propagatedBuildInputs = [
      py.tree-sitter
      py.tree-sitter-c-sharp
      py.tree-sitter-embedded-template
      py.tree-sitter-yaml
    ];

    doCheck = false;
  };

  cargoDeps = rustPlatform.fetchCargoVendor {
    inherit src;
    hash = "sha256-hTbTJrnoJOHTxvT5uusCNIA30cYU4N8Ixen4y1rExyg=";
  };

in py.buildPythonApplication rec {
  pname = "headroom-ai";
  version = "0.41.0";
  format = "pyproject";

  inherit src;

  inherit cargoDeps;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.cargoSetupHook
    rustPlatform.maturinBuildHook
  ];

  buildInputs = [
    onnxruntime
  ];

  # System ONNX Runtime configuration for native fastembed/ort crates
  ORT_STRATEGY = "system";
  ORT_LIB_LOCATION = "${onnxruntime}/lib";
  ORT_LIB_DIR = "${onnxruntime}/lib";

  propagatedBuildInputs = [
    # Core dependencies
    py.tiktoken
    py.pydantic
    py.litellm
    py.click
    py.rich
    py.opentelemetry-api
    py.ast-grep-cli
    py.pyyaml
    py.tomlkit
    py.truststore
    py.hf-xet

    # Proxy extra
    py.fastapi
    py.uvicorn
    py.orjson
    py.httpx
    py.h2
    py.openai
    mcp_1_30
    py.magika
    py.zstandard
    py.websockets
    py.onnxruntime
    py.transformers
    py.watchdog
    py.sqlite-vec

    # Code / Tree-sitter extra
    tree-sitter-language-pack_0_13
    py.tree-sitter
  ];

  pythonImportsCheck = [
    "headroom"
  ];

  meta = with lib; {
    description = "The Context Optimization Layer for LLM Applications";
    homepage = "https://docs.headroomlabs.ai";
    license = licenses.asl20;
    maintainers = [ ];
    mainProgram = "headroom";
  };
}
