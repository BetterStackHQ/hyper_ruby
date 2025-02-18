# frozen_string_literal: true

require "bundler/gem_tasks"
require "rake/testtask"

Rake::TestTask.new(:test) do |t|
  t.libs << "test"
  t.libs << "lib"
  t.pattern = "test/**/test_*.rb"
  t.warning = false
  t.verbose = true
end

# Remove the existing default task
Rake::Task[:default].clear if Rake::Task.task_defined?(:default)

namespace :proto do
  desc "Generate Ruby code from proto files"
  task :generate do
    system("grpc_tools_ruby_protoc -I test --ruby_out=test --grpc_out=test test/echo.proto") or fail "Failed to generate proto files"
  end
end

require "rb_sys/extensiontask"

task build: :compile

GEMSPEC = Gem::Specification.load("hyper_ruby.gemspec")

RbSys::ExtensionTask.new("hyper_ruby", GEMSPEC) do |ext|
  ext.lib_dir = "lib/hyper_ruby"
end

# Define the default task to run both compile and test
task :default => [:compile, :test]
