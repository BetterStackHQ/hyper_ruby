# frozen_string_literal: true
# Generated by the protocol buffer compiler.  DO NOT EDIT!
# source: echo.proto

require 'google/protobuf'


descriptor_data = "\n\necho.proto\x12\x04\x65\x63ho\"\x1e\n\x0b\x45\x63hoRequest\x12\x0f\n\x07message\x18\x01 \x01(\t\"\x1f\n\x0c\x45\x63hoResponse\x12\x0f\n\x07message\x18\x01 \x01(\t27\n\x04\x45\x63ho\x12/\n\x04\x45\x63ho\x12\x11.echo.EchoRequest\x1a\x12.echo.EchoResponse\"\x00\x62\x06proto3"

pool = Google::Protobuf::DescriptorPool.generated_pool
pool.add_serialized_file(descriptor_data)

module Echo
  EchoRequest = ::Google::Protobuf::DescriptorPool.generated_pool.lookup("echo.EchoRequest").msgclass
  EchoResponse = ::Google::Protobuf::DescriptorPool.generated_pool.lookup("echo.EchoResponse").msgclass
end
