# frozen_string_literal: true

require_relative "lib/hyper_ruby/version"

Gem::Specification.new do |spec|
  spec.name = "hyper_ruby"
  spec.version = HyperRuby::VERSION
  spec.authors = ["alistairjevans"]
  spec.email = ["alistair@betterstack.com"]

  spec.summary = "Hyper-backed ruby web server"
  spec.description = "Hyper-backed ruby web server"
  spec.homepage = "https://github.com/betterstack/hyper-ruby"
  spec.license = "MIT"
  spec.required_ruby_version = ">= 3.0.0"
  spec.required_rubygems_version = ">= 3.3.11"

  spec.metadata["allowed_push_host"] = "TODO: Set to your gem server 'https://example.com'"

  spec.metadata["homepage_uri"] = spec.homepage
  spec.metadata["source_code_uri"] = "https://github.com/betterstack/hyper-ruby"
  spec.metadata["changelog_uri"] = "https://github.com/betterstack/hyper-ruby/blob/main/CHANGELOG.md"

  # Specify which files should be added to the gem when it is released.
  # The `git ls-files -z` loads the files in the RubyGem that have been added into git.
  gemspec = File.basename(__FILE__)
  spec.files = IO.popen(%w[git ls-files -z], chdir: __dir__, err: IO::NULL) do |ls|
    ls.readlines("\x0", chomp: true).reject do |f|
      (f == gemspec) ||
        f.start_with?(*%w[bin/ test/ spec/ features/ .git .github appveyor Gemfile])
    end
  end
  spec.bindir = "exe"
  spec.executables = spec.files.grep(%r{\Aexe/}) { |f| File.basename(f) }
  spec.require_paths = ["lib"]
  spec.extensions = ["ext/hyper_ruby/Cargo.toml"]

  # Uncomment to register a new dependency of your gem
  # spec.add_dependency "example-gem", "~> 1.0"

  # For more information and examples about making a new gem, check out our
  # guide at: https://bundler.io/guides/creating_gem.html
end
