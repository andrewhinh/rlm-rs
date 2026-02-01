import { defineConfig } from "eslint/config";
import js from "@eslint/js";
import svelte from "eslint-plugin-svelte";
import globals from "globals";
import ts from "typescript-eslint";

import prettier from "eslint-config-prettier";

import svelteConfig from "./svelte.config.js";

export default defineConfig([
  {
    ignores: [
      ".svelte-kit/**",
      "build/**",
      "node_modules/**",
      "crates/**",
      "**/target/**",
    ],
  },

  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs["flat/recommended"],

  {
    languageOptions: {
      globals: {
        ...globals.browser,
        ...globals.node,
      },
    },
  },

  {
    files: ["**/*.svelte", "**/*.svelte.ts", "**/*.svelte.js"],
    languageOptions: {
      parserOptions: {
        extraFileExtensions: [".svelte"],
        parser: ts.parser,
        svelteConfig,
      },
    },
    rules: {
      "no-undef": "off",
    },
  },

  ...svelte.configs["flat/prettier"].slice(-1),
  prettier,

  {
    files: ["**/*.cjs"],
    languageOptions: {
      sourceType: "commonjs",
    },
    rules: {
      "@typescript-eslint/no-require-imports": "off",
    },
  },

  {
    rules: {
      "@typescript-eslint/ban-ts-comment": "off",
      "@typescript-eslint/no-empty-function": "off",
      "@typescript-eslint/no-explicit-any": "off",
      "@typescript-eslint/no-non-null-assertion": "off",
      "no-constant-condition": "off",
      "no-control-regex": "off",
      "no-empty": "off",
    },
  },

  {
    files: ["src/routes/+page.svelte"],
    rules: {
      "svelte/no-dom-manipulating": "off",
    },
  },
]);
