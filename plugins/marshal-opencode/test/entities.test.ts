// Contract test for the hand-mirrored marshal wire shapes. These assertions
// encode the exact JSON the Rust `marshal-entities` daemon expects, so an edit
// to entities.ts that drifts from the Rust serde fails here rather than at
// runtime against a live daemon.
//
// Run: bun test

import { describe, expect, test } from "bun:test";

import {
  ackMessages,
  broadcastMessage,
  getAllRoomMembers,
  getAllRooms,
  getAllSessions,
  joinRoom,
  leaveRoom,
  readMessages,
  sendMessage,
  setLastActivityAt,
  setSessionStatus,
} from "../src/entities.js";

const SID = "ses_abc";

describe("command ids match the Rust struct names verbatim", () => {
  test("write commands", () => {
    expect(sendMessage(SID, "ses_to", "hi").commandId).toBe("SendMessage");
    expect(broadcastMessage(SID, "everyone", "hi").commandId).toBe("BroadcastMessage");
    expect(joinRoom(SID, "war-room").commandId).toBe("JoinRoom");
    expect(leaveRoom(SID, "war-room").commandId).toBe("LeaveRoom");
    expect(ackMessages(SID, ["m1"]).commandId).toBe("AckMessages");
    expect(readMessages({ asSession: SID }).commandId).toBe("ReadMessages");
    expect(setSessionStatus(SID, "busy").commandId).toBe("SetSessionCurrentTask");
    expect(setLastActivityAt(SID, 1).commandId).toBe("SetSessionLastActivityAt");
  });
});

describe("query ids and item types match the Rust generated queries", () => {
  test("GetAll* queries", () => {
    expect(getAllSessions()).toMatchObject({ queryId: "GetAllSessions", queryItemType: "Session", query: {} });
    expect(getAllRooms()).toMatchObject({ queryId: "GetAllRooms", queryItemType: "Room" });
    expect(getAllRoomMembers()).toMatchObject({ queryId: "GetAllRoomMembers", queryItemType: "RoomMember" });
  });
});

describe("command payloads are camelCase and carry asSession", () => {
  test("SendMessage", () => {
    expect(sendMessage(SID, "ses_to", "hello").command).toEqual({
      toSessionId: "ses_to",
      body: "hello",
      asSession: SID,
    });
  });

  test("BroadcastMessage", () => {
    expect(broadcastMessage(SID, "host:nyc", "hello").command).toEqual({
      toRoomId: "host:nyc",
      body: "hello",
      asSession: SID,
    });
  });

  test("JoinRoom omits an undefined description", () => {
    expect(joinRoom(SID, "war-room").command).toEqual({ name: "war-room", asSession: SID });
    expect(joinRoom(SID, "war-room", "the room").command).toEqual({
      name: "war-room",
      description: "the room",
      asSession: SID,
    });
  });

  test("AckMessages takes a messageIds array", () => {
    expect(ackMessages(SID, ["m1", "m2"]).command).toEqual({ messageIds: ["m1", "m2"], asSession: SID });
  });

  test("SetSessionCurrentTask is the Session.current_task setter payload", () => {
    expect(setSessionStatus(SID, "busy").command).toEqual({ id: SID, currentTask: "busy" });
  });

  test("ReadMessages always sends the non-Option bool filters", () => {
    expect(readMessages({ asSession: SID, inbox: true, unread: true, limit: 20 }).command).toEqual({
      asSession: SID,
      inbox: true,
      sent: false,
      unread: true,
      limit: 20,
    });
  });
});
