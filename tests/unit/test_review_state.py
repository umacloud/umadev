from pathlib import Path

from super_dev.review_state import (
    latest_workflow_snapshot_file,
    load_recent_workflow_snapshots,
    load_workflow_state,
    save_workflow_state,
    workflow_state_file,
)


def test_load_workflow_state_falls_back_to_latest_snapshot(temp_project_dir: Path) -> None:
    payload = {
        "status": "waiting_preview_confirmation",
        "workflow_mode": "revise",
        "current_step_label": "等待前端预览确认",
        "recommended_command": 'super-dev review preview --status confirmed --comment "前端预览已确认"',
    }
    save_workflow_state(temp_project_dir, payload)
    workflow_state_file(temp_project_dir).write_text("{invalid", encoding="utf-8")

    restored = load_workflow_state(temp_project_dir)

    assert restored is not None
    assert restored["status"] == "waiting_preview_confirmation"
    assert restored["current_step_label"] == "等待前端预览确认"
    assert latest_workflow_snapshot_file(temp_project_dir).exists()


def test_load_recent_workflow_snapshots_returns_latest_first(temp_project_dir: Path) -> None:
    save_workflow_state(
        temp_project_dir,
        {
            "status": "waiting_docs_confirmation",
            "workflow_mode": "revise",
            "current_step_label": "等待三文档确认",
            "recommended_command": 'super-dev review docs --status confirmed --comment "三文档已确认"',
        },
    )
    save_workflow_state(
        temp_project_dir,
        {
            "status": "waiting_preview_confirmation",
            "workflow_mode": "revise",
            "current_step_label": "等待前端预览确认",
            "recommended_command": 'super-dev review preview --status confirmed --comment "前端预览已确认"',
        },
    )

    snapshots = load_recent_workflow_snapshots(temp_project_dir, limit=2)

    assert len(snapshots) == 2
    assert snapshots[0]["current_step_label"] == "等待前端预览确认"
    assert snapshots[1]["current_step_label"] == "等待三文档确认"
    assert snapshots[0]["path"].endswith(".json")
