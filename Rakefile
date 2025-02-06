# frozen_string_literal: true

require "bundler/gem_tasks"
require "minitest/test_task"

Minitest::TestTask.create

require "rb_sys/extensiontask"

task build: :compile

GEMSPEC = Gem::Specification.load("hyper_ruby.gemspec")

RbSys::ExtensionTask.new("hyper_ruby", GEMSPEC) do |ext|
  ext.lib_dir = "lib/hyper_ruby"
end

task default: %i[compile test]
